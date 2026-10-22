#!/bin/sh
# Build immutable static Linux helpers. Does not package, sign, install or launch the app.
# Usage: scripts/build-remote-host.sh [--output DIRECTORY]
set -eu
cd "$(dirname "$0")/.."
TASK_REPOSITORY=$(pwd -P)
# Official multiarch Rust 1.96 index, verified locally to contain rustc 1.96.1.
# Static musl removes the builder's glibc dependency; Debian 12 executes every output below.
BUILDER_IMAGE=${DEPPY_REMOTE_HOST_BUILDER_IMAGE:-rust@sha256:1f0dbad1df66647807e6952d1db85d0b2bda7606cb2139d82517e4f009967376}
BASELINE_IMAGE=debian@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
HELPER_OUTPUT="target/remote-host/$VERSION"
case "$#" in
    0) ;;
    2) [ "$1" = "--output" ] || { echo "usage: build-remote-host.sh [--output DIRECTORY]" >&2; exit 2; }; HELPER_OUTPUT=$2 ;;
    *) echo "usage: build-remote-host.sh [--output DIRECTORY]" >&2; exit 2 ;;
esac
[ ! -e "$HELPER_OUTPUT" ] && [ ! -L "$HELPER_OUTPUT" ] || { echo "helper output already exists" >&2; exit 1; }
mkdir -p "$TASK_REPOSITORY/target"
TASK_STAGE=$(mktemp -d "$TASK_REPOSITORY/target/.remote-host-build.XXXXXX")
TASK_CONTAINER=""
TASK_TARGET_CACHE=""
TASK_CARGO_CACHE=""
TASK_MOUNT_TOKEN=""
cleanup() {
    if [ -n "$TASK_CONTAINER" ]; then docker rm --force "$TASK_CONTAINER" >/dev/null 2>&1 || true; fi
    for directory in "$TASK_TARGET_CACHE" "$TASK_CARGO_CACHE"; do
        if [ -n "$directory" ] && [ -n "$TASK_MOUNT_TOKEN" ]; then
            rm -f "$directory/.deppy-host-$TASK_MOUNT_TOKEN" "$directory/.deppy-container-$TASK_MOUNT_TOKEN"
        fi
    done
    rm -rf "$TASK_STAGE"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
for label in linux-aarch64 linux-x86_64; do
    case "$label" in
        linux-aarch64) platform=linux/arm64; architecture=aarch64 ;;
        linux-x86_64) platform=linux/amd64; architecture=x86_64 ;;
    esac
    target="$architecture-unknown-linux-musl"
    # Host bind mounts keep Cargo caches out of Docker's small engine disk.
    TASK_TARGET_CACHE="$TASK_REPOSITORY/target-ssh-musl-$architecture"
    TASK_CARGO_CACHE="$TASK_REPOSITORY/target-ssh-musl-cargo-$architecture"
    mkdir -p "$TASK_TARGET_CACHE" "$TASK_CARGO_CACHE" "$TASK_STAGE/$label"
    TASK_MOUNT_TOKEN=$(python3 -c 'import secrets; print(secrets.token_hex(24))')
    TASK_CONTAINER="deppy-remote-host-build-$TASK_MOUNT_TOKEN"
    for directory in "$TASK_TARGET_CACHE" "$TASK_CARGO_CACHE" "$TASK_STAGE/$label"; do
        printf '%s\n' "$TASK_MOUNT_TOKEN" > "$directory/.deppy-host-$TASK_MOUNT_TOKEN"
    done
    python3 scripts/run-remote-host-container.py --container "$TASK_CONTAINER" --seconds 2700 -- \
        docker run --rm -i --name "$TASK_CONTAINER" --platform "$platform" \
        --tmpfs /var/cache/apt/archives:rw,size=128m \
        --mount "type=bind,src=$TASK_REPOSITORY,dst=/workspace,readonly" \
        --mount "type=bind,src=$TASK_TARGET_CACHE,dst=/target" \
        --mount "type=bind,src=$TASK_CARGO_CACHE,dst=/cargo" \
        --mount "type=bind,src=$TASK_STAGE/$label,dst=/artifacts" \
        -e "DEPPY_HELPER_TARGET=$target" -e "DEPPY_HELPER_ARCH=$architecture" -e "DEPPY_HELPER_VERSION=$VERSION" \
        -e "DEPPY_MOUNT_TOKEN=$TASK_MOUNT_TOKEN" \
        -e CARGO_TARGET_DIR=/target -e CARGO_HOME=/cargo -e CARGO_BUILD_JOBS=2 \
        -e "RUSTUP_TOOLCHAIN=1.96.1-$architecture-unknown-linux-gnu" \
        -w /workspace "$BUILDER_IMAGE" sh -eu <<'BUILD_REMOTE_HELPER'
            for directory in /target /cargo /artifacts; do
                [ "$(cat "$directory/.deppy-host-$DEPPY_MOUNT_TOKEN")" = "$DEPPY_MOUNT_TOKEN" ] || { echo "Docker mount is not shared with host" >&2; exit 1; }
                printf "%s\n" "$DEPPY_MOUNT_TOKEN" > "$directory/.deppy-container-$DEPPY_MOUNT_TOKEN"
            done
            case "$(rustc --version)" in "rustc 1.96.1 "*) ;; *) echo "wrong Rust toolchain" >&2; exit 1;; esac
            apt-get update -qq
            apt-get install -y --no-install-recommends musl-tools
            rustup target add "$DEPPY_HELPER_TARGET"
            case "$DEPPY_HELPER_ARCH" in
                aarch64) export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc CC_aarch64_unknown_linux_musl=musl-gcc ;;
                x86_64) export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc CC_x86_64_unknown_linux_musl=musl-gcc ;;
            esac
            cargo build --locked --release -p deppy-remote-host --target "$DEPPY_HELPER_TARGET"
            cargo test --locked --release -p storage --lib --jobs 1 --target "$DEPPY_HELPER_TARGET" --no-run --message-format=json > /artifacts/storage-tests-build.json
            storage_test=$(sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' /artifacts/storage-tests-build.json)
            case "$storage_test" in "/target/$DEPPY_HELPER_TARGET/release/deps/storage-"*) ;; *) echo "storage test artifact missing" >&2; exit 1;; esac
            cp "$storage_test" /artifacts/storage-tests
            chmod 755 /artifacts/storage-tests
            binary="/target/$DEPPY_HELPER_TARGET/release/deppy-remote-host"
            [ "$("$binary" --version)" = "deppy-remote-host $DEPPY_HELPER_VERSION" ] || { echo "helper version mismatch" >&2; exit 1; }
            cp "$binary" /artifacts/deppy-remote-host
            chmod 755 /artifacts/deppy-remote-host
BUILD_REMOTE_HELPER
    for directory in "$TASK_TARGET_CACHE" "$TASK_CARGO_CACHE" "$TASK_STAGE/$label"; do
        [ "$(cat "$directory/.deppy-container-$TASK_MOUNT_TOKEN")" = "$TASK_MOUNT_TOKEN" ] || { echo "Docker output is not visible on host" >&2; exit 1; }
        rm -f "$directory/.deppy-host-$TASK_MOUNT_TOKEN" "$directory/.deppy-container-$TASK_MOUNT_TOKEN"
    done
    [ -f "$TASK_STAGE/$label/deppy-remote-host" ] && [ ! -L "$TASK_STAGE/$label/deppy-remote-host" ] || { echo "helper output not visible on host" >&2; exit 1; }
    TASK_CONTAINER="deppy-remote-host-baseline-$TASK_MOUNT_TOKEN"
    reported=$(python3 scripts/run-remote-host-container.py --container "$TASK_CONTAINER" --seconds 300 -- \
        docker run --rm --name "$TASK_CONTAINER" --platform "$platform" \
        --mount "type=bind,src=$TASK_STAGE/$label,dst=/helper,readonly" \
        --entrypoint /helper/deppy-remote-host "$BASELINE_IMAGE" --version)
    [ "$reported" = "deppy-remote-host $VERSION" ] || { echo "Debian 12 helper execution failed" >&2; exit 1; }
    # Run permission-sensitive archive tests as an unprivileged user on the baseline.
    TASK_CONTAINER="deppy-remote-host-storage-$TASK_MOUNT_TOKEN"
    python3 scripts/run-remote-host-container.py --container "$TASK_CONTAINER" --seconds 300 -- \
        docker run --rm --name "$TASK_CONTAINER" --platform "$platform" --user 65534:65534 \
        --tmpfs /tmp:rw,size=256m,mode=1777 \
        --mount "type=bind,src=$TASK_STAGE/$label,dst=/helper,readonly" \
        --entrypoint /helper/storage-tests "$BASELINE_IMAGE" scrollback_archive --nocapture
    TASK_CONTAINER=""
done
python3 scripts/prepare-remote-host.py --source "$TASK_STAGE" --output "$HELPER_OUTPUT" --version "$VERSION" --create-manifest
echo "verified static Linux helper artifacts: $HELPER_OUTPUT"
