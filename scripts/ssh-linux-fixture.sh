#!/bin/bash
# Isolated Linux/OpenSSH test server. Never reads or changes ~/.ssh.
set -euo pipefail
umask 077

action=${1:-start}
state=${2:-}
platform=${3:-linux/arm64}
project=$(cd "$(dirname "$0")/.." && pwd)
image=deppy-ssh-linux-fixture:20261022

if [[ "$action" == stop ]]; then
  [[ -n "$state" && -f "$state/container-id" && ! -L "$state/container-id" ]]
  container_id=$(cat "$state/container-id")
  [[ "$container_id" =~ ^[0-9a-f]{64}$ ]]
  docker rm -f "$container_id" >/dev/null
  rm -f "$state/container-id" "$state/client_key" "$state/host_key"
  printf 'Stopped isolated fixture in %s\n' "$state"
  exit 0
fi
if [[ "$action" != start ]]; then
  printf 'Usage: %s start [private-state-dir] [linux/arm64|linux/amd64]\n       %s stop private-state-dir\n' "$0" "$0" >&2
  exit 2
fi
case "$platform" in
  linux/arm64) architecture=arm64; toolchain=1.96.1-aarch64-unknown-linux-gnu ;;
  linux/amd64) architecture=x86_64; toolchain=1.96.1-x86_64-unknown-linux-gnu ;;
  *) printf 'Unsupported fixture platform\n' >&2; exit 2 ;;
esac
if [[ -z "$state" ]]; then state=$(mktemp -d "$project/../.deppy-linux-ssh.XXXXXX"); fi
mkdir -p "$state"
[[ -d "$state" && ! -L "$state" && ! -e "$state/container-id" ]]
chmod 700 "$state"
state=$(cd "$state" && pwd)
if [[ $(docker context show) == colima && "$state" != /Users/* ]]; then
  printf 'Colima fixture state must be on its shared /Users mount.\n' >&2
  exit 2
fi
volume=deppy-ssh-linux-${architecture}-target-20261022
target_mount=$volume
host_target=$project/target-linux-${architecture}
if [[ -d "$host_target" ]]; then target_mount=$host_target; fi

# Investigation binary only. Releases must choose their supported libc baseline.
docker run --rm --platform "$platform" --network=none \
  -v "$project":/workspace:ro -v "$HOME/.cargo/registry":/cargo/registry:ro \
  -v "$target_mount":/target -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target \
  -e CARGO_BUILD_JOBS=2 -e RUSTUP_TOOLCHAIN="$toolchain" -w /workspace rust:1.96 \
  cargo build --offline --locked -p deppy-remote-host
version_output=$(docker run --rm --platform "$platform" --network=none \
  -v "$target_mount":/target:ro rust:1.96 /target/debug/deppy-remote-host --version)
[[ "$version_output" =~ ^deppy-remote-host\ ([0-9]+\.[0-9]+\.[0-9]+)$ ]]
helper_version=${BASH_REMATCH[1]}

cat > "$state/Dockerfile" <<'EOF'
FROM rust:1.96
RUN --mount=type=tmpfs,target=/var/cache/apt/archives apt-get update && apt-get install -y --no-install-recommends openssh-server && \
    rm -rf /var/lib/apt/lists/* && useradd --create-home --shell /bin/sh deppy && \
    passwd -d deppy && mkdir -p /run/sshd
EXPOSE 22
EOF
docker build --platform "$platform" -t "$image" -f "$state/Dockerfile" "$state"
ssh-keygen -q -t ed25519 -N '' -C deppy-isolated-fixture-client -f "$state/client_key"
ssh-keygen -q -t ed25519 -N '' -C deppy-isolated-fixture-host -f "$state/host_key"
cat > "$state/sshd_config" <<'EOF'
Port 22
ListenAddress 0.0.0.0
HostKey /fixture/host_key
PidFile /run/deppy-fixture-sshd.pid
AuthorizedKeysFile .ssh/authorized_keys
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitEmptyPasswords no
PermitRootLogin no
UsePAM no
AllowUsers deppy
StrictModes yes
AllowTcpForwarding no
X11Forwarding no
PermitTunnel no
PrintMotd no
LogLevel ERROR
EOF
cat > "$state/entrypoint.sh" <<'EOF'
#!/bin/sh
set -eu
umask 077
install -d -m 700 -o deppy -g deppy /home/deppy
install -d -m 700 -o deppy -g deppy /home/deppy/.ssh
install -m 600 -o deppy -g deppy /fixture/client_key.pub /home/deppy/.ssh/authorized_keys
install -d -m 700 -o deppy -g deppy "/home/deppy/.local/share/deppy/remote-host/$HELPER_VERSION"
install -m 755 -o deppy -g deppy /build-target/debug/deppy-remote-host "/home/deppy/.local/share/deppy/remote-host/$HELPER_VERSION/deppy-remote-host"
install -d -m 700 -o deppy -g deppy /home/deppy/workspace
printf 'remote fixture document\n' > /home/deppy/workspace/document.txt
git -C /home/deppy/workspace init -q
chown -R deppy:deppy /home/deppy/workspace
exec /usr/sbin/sshd -D -e -f /fixture/sshd_config
EOF
chmod 700 "$state/entrypoint.sh"
docker run -d --platform "$platform" --label deppy.test-fixture=ssh-linux \
  --tmpfs /home/deppy:rw,exec,size=512m,mode=0755 \
  -p 127.0.0.1::22 -v "$state":/fixture:ro -v "$target_mount":/build-target:ro \
  -e HELPER_VERSION="$helper_version" --entrypoint /fixture/entrypoint.sh "$image" \
  > "$state/container-id"
container_id=$(cat "$state/container-id")
port=$(docker port "$container_id" 22/tcp | sed -n 's/^127\.0\.0\.1://p')
[[ "$port" =~ ^[0-9]+$ ]]
printf 'deppy-linux-fixture %s\n' "$(cat "$state/host_key.pub")" > "$state/known_hosts"
cat > "$state/ssh_config" <<EOF
Host deppy-linux-fixture
    HostName 127.0.0.1
    Port $port
    User deppy
    IdentityFile "$state/client_key"
    IdentitiesOnly yes
    UserKnownHostsFile "$state/known_hosts"
    GlobalKnownHostsFile /dev/null
    HostKeyAlias deppy-linux-fixture
    StrictHostKeyChecking yes
    BatchMode yes
    LogLevel ERROR
EOF
printf '#!/bin/bash\nexec /usr/bin/ssh -F %q "$@"\n' "$state/ssh_config" > "$state/ssh"
chmod 700 "$state/ssh"
ready=0
for attempt in $(seq 1 100); do
  if probe=$("$state/ssh" deppy-linux-fixture \
    "test -w \"\$HOME\" && exec \"\$HOME/.local/share/deppy/remote-host/$helper_version/deppy-remote-host\" --version" 2>/dev/null) && \
    [[ "$probe" == "deppy-remote-host $helper_version" ]]; then ready=1; break; fi
  sleep 0.1
done
if [[ "$ready" != 1 ]]; then docker logs "$container_id" >&2; exit 1; fi
fingerprint=$(ssh-keygen -lf "$state/host_key.pub" -E sha256 | awk '{print $2}')
python3 - "$state" "$port" "$helper_version" "$fingerprint" "$platform" <<'PY'
import json, pathlib, sys
state, port, version, fingerprint, platform = sys.argv[1:]
data = {"destination": "deppy-linux-fixture", "account": "deppy", "host": "127.0.0.1", "port": int(port),
        "workspace": "/home/deppy/workspace", "helper_version": version, "host_key_sha256": fingerprint,
        "ssh_program": str(pathlib.Path(state) / "ssh"), "platform": platform}
pathlib.Path(state, "fixture.json").write_text(json.dumps(data, indent=2) + "\n")
print(json.dumps(data, indent=2))
PY
printf 'Private state: %s\nCleanup: bash %q stop %q\n' "$state" "$0" "$state"
