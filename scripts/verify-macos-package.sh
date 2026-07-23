#!/bin/sh
# Deterministic macOS bundle/archive verification. Production trust is required by default.
# Inspecting an ad-hoc or local self-signed development bundle requires both
# DEPPY_REQUIRE_TRUSTED_SIGNING=0 and DEPPY_ALLOW_UNTRUSTED_SIGNING=1.
set -eu

cd "$(dirname "$0")/.."

BUNDLE=${1:?usage: verify-macos-package.sh BUNDLE [ARCHIVE]}
ARCHIVE=${2:-}
APP_NAME="Deppy Sijo"
BIN_NAME="deppy-sijo"
PROXY_NAME="deppy-mcp-proxy"
EXPECTED_BUNDLE_ID="app.vector9.deppy-sijo"
EXPECTED_VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')
REQUIRED_ARCHS=${DEPPY_REQUIRED_ARCHS:-$(uname -m)}
REQUIRE_TRUSTED=${DEPPY_REQUIRE_TRUSTED_SIGNING:-1}
ALLOW_UNTRUSTED=${DEPPY_ALLOW_UNTRUSTED_SIGNING:-0}

case "$REQUIRE_TRUSTED:$ALLOW_UNTRUSTED" in
    1:0 | 1:1 | 0:1) ;;
    *)
        echo "package verification failed: untrusted verification requires DEPPY_ALLOW_UNTRUSTED_SIGNING=1" >&2
        exit 1
        ;;
esac

fail() {
    echo "package verification failed: $*" >&2
    exit 1
}

plist_value() {
    /usr/libexec/PlistBuddy -c "Print :$2" "$1/Contents/Info.plist" 2>/dev/null
}

verify_binary() {
    binary=$1
    [ -f "$binary" ] || fail "missing binary: $binary"
    [ -x "$binary" ] || fail "binary is not executable: $binary"
    codesign --verify --strict --verbose=2 "$binary"
    archs=$(lipo -archs "$binary")
    for required_arch in $REQUIRED_ARCHS; do
        case " $archs " in
            *" $required_arch "*) ;;
            *) fail "missing architecture $required_arch in $binary (found: $archs)" ;;
        esac
    done
}

signature_info() {
    codesign -dv --verbose=4 "$1" 2>&1
}

team_identifier() {
    signature_info "$1" | sed -n 's/^TeamIdentifier=//p' | head -1
}

verify_trusted_code() {
    code=$1
    expected_team=$2
    info=$(signature_info "$code")
    team_id=$(echo "$info" | sed -n 's/^TeamIdentifier=//p' | head -1)
    [ "$team_id" = "$expected_team" ] || fail "signing team mismatch: $code"
    echo "$info" | grep -q '^Authority=Developer ID Application:' || fail "Developer ID Application signature required: $code"
    echo "$info" | grep -q '^Signature=adhoc$' && fail "ad-hoc signature cannot pass production gate: $code"
    echo "$info" | grep -q 'flags=.*runtime' || fail "hardened runtime missing: $code"
    timestamp=$(echo "$info" | sed -n 's/^Timestamp=//p' | head -1)
    [ -n "$timestamp" ] && [ "$timestamp" != "none" ] || fail "trusted timestamp missing: $code"
    requirement="anchor apple generic and certificate leaf[subject.OU] = \"$expected_team\" and certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
    codesign --verify --strict --verbose=2 -R="$requirement" "$code"
}

verify_bundle() {
    candidate=$1
    [ -d "$candidate" ] || fail "missing bundle: $candidate"
    [ "$(plist_value "$candidate" CFBundlePackageType)" = "APPL" ] || fail "invalid package type"
    [ "$(plist_value "$candidate" CFBundleIdentifier)" = "$EXPECTED_BUNDLE_ID" ] || fail "invalid bundle identifier"
    [ "$(plist_value "$candidate" CFBundleExecutable)" = "$BIN_NAME" ] || fail "invalid executable name"
    [ "$(plist_value "$candidate" CFBundleVersion)" = "$EXPECTED_VERSION" ] || fail "invalid bundle version"
    [ "$(plist_value "$candidate" CFBundleShortVersionString)" = "$EXPECTED_VERSION" ] || fail "invalid short version"
    [ "$(plist_value "$candidate" LSMinimumSystemVersion)" = "11.0" ] || fail "invalid minimum macOS version"

    verify_binary "$candidate/Contents/MacOS/$BIN_NAME"
    verify_binary "$candidate/Contents/MacOS/$PROXY_NAME"
    codesign --verify --deep --strict --verbose=2 "$candidate"

    if [ "$REQUIRE_TRUSTED" = "1" ]; then
        team_id=$(team_identifier "$candidate")
        case "$team_id" in
            '' | *[!A-Z0-9]*) fail "trusted TeamIdentifier is invalid" ;;
        esac
        [ "${#team_id}" -eq 10 ] || fail "trusted TeamIdentifier length is invalid"
        verify_trusted_code "$candidate" "$team_id"
        verify_trusted_code "$candidate/Contents/MacOS/$BIN_NAME" "$team_id"
        verify_trusted_code "$candidate/Contents/MacOS/$PROXY_NAME" "$team_id"
    fi
}

verify_bundle "$BUNDLE"

if [ -n "$ARCHIVE" ]; then
    [ -f "$ARCHIVE" ] || fail "missing archive: $ARCHIVE"
    VERIFY_TMP_DIR=$(mktemp -d /tmp/deppy-package-verify.XXXXXX)
    trap 'rm -rf "$VERIFY_TMP_DIR"' EXIT HUP INT TERM
    ditto -x -k "$ARCHIVE" "$VERIFY_TMP_DIR"
    EXTRACTED_BUNDLE="$VERIFY_TMP_DIR/$APP_NAME.app"
    verify_bundle "$EXTRACTED_BUNDLE"
    for binary_name in "$BIN_NAME" "$PROXY_NAME"; do
        original_hash=$(shasum -a 256 "$BUNDLE/Contents/MacOS/$binary_name" | awk '{print $1}')
        archived_hash=$(shasum -a 256 "$EXTRACTED_BUNDLE/Contents/MacOS/$binary_name" | awk '{print $1}')
        [ "$original_hash" = "$archived_hash" ] || fail "archive changed $binary_name"
    done
fi

if [ "$REQUIRE_TRUSTED" = "1" ]; then
    echo "package verification OK: trusted bundle, helper, plist, architecture, signature, archive"
else
    echo "package verification OK: explicitly untrusted development bundle"
fi
