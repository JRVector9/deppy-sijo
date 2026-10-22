#!/usr/bin/env python3
"""Check the settings transfer's bytes and scope; this does not run Rust tests."""

import argparse
import gzip
import hashlib
import json
import subprocess
from collections import Counter
from pathlib import Path


def digest(data):
    return hashlib.sha256(data).hexdigest()


def git(root, *args):
    return subprocess.check_output(["git", *args], cwd=root)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[4])
    parser.add_argument("--check-source", action="store_true")
    parser.add_argument("--with-lifecycle", action="store_true", help="reverse the separately authorized lifecycle overlay before checking original transfer bytes")
    args = parser.parse_args()
    manifest_path = Path(__file__).with_name("transfer-manifest.json")
    manifest = json.loads(manifest_path.read_text())
    root = args.root.resolve()
    files = manifest["files"]
    paths = {row["path"] for row in files}
    assert len(files) == len(paths) == 27, "approved file inventory must contain exactly 27 paths"
    expected_modes = {"exact_copy": 21, "scoped_app_patch": 1, "scoped_locale_append": 5}
    assert dict(Counter(row["mode"] for row in files)) == expected_modes
    base = manifest["integration_base"]
    projected_files = {}
    overlay_extra_paths = set()
    if args.with_lifecycle:
        overlay = json.loads(manifest_path.with_name("lifecycle-overlay-manifest.json").read_text())
        assert overlay["base_transfer_manifest_sha256"] == digest(manifest_path.read_bytes())
        assert len(overlay["files"]) == 3
        for row in overlay["files"]:
            path = row["path"]
            actual = (root / path).read_bytes()
            assert digest(actual) == row["final_sha256"], f"lifecycle digest changed: {path}"
            for operation in reversed(row["operations"]):
                before = operation["before_text"].encode()
                after = operation["after_text"].encode()
                assert after and actual.count(after) == 1, f"lifecycle reversal is ambiguous: {path}"
                actual = actual.replace(after, before, 1)
            assert digest(actual) == row["before_overlay_sha256"], f"lifecycle changed unrelated bytes: {path}"
            projected_files[path] = actual
            if path not in paths:
                assert actual == git(root, "show", f"{base}:{path}"), f"extra lifecycle file changed unrelated base bytes: {path}"
                overlay_extra_paths.add(path)
        assert overlay_extra_paths == {"crates/app/src/keychain_startup_tests.rs"}
        final_manifest = json.loads(manifest_path.with_name("final-source-manifest.json").read_text())
        assert final_manifest["transfer_manifest_sha256"] == digest(manifest_path.read_bytes())
        assert final_manifest["lifecycle_overlay_manifest_sha256"] == digest(manifest_path.with_name("lifecycle-overlay-manifest.json").read_bytes())
        final_paths = {row["path"] for row in final_manifest["files"]}
        assert len(final_manifest["files"]) == len(final_paths) == 28
        assert final_paths == paths | overlay_extra_paths
        for row in final_manifest["files"]:
            assert digest((root / row["path"]).read_bytes()) == row["sha256"], f"final inventory digest differs: {row['path']}"
        for label, passed, failed, exit_code in [("lifecycle-red", 0, 2, 101), ("lifecycle-green", 2, 0, 0)]:
            execution = json.loads(manifest_path.with_name(f"{label}-execution.json").read_text())
            raw_log = gzip.decompress(manifest_path.with_name(f"{label}-execution.log.gz").read_bytes())
            assert digest(raw_log) == execution["log_sha256"]
            assert execution["exit"] == exit_code
            assert any(f"{passed} passed; {failed} failed;" in summary for summary in execution["summaries"])
    for row in files:
        path = row["path"]
        actual = projected_files.get(path)
        if actual is None:
            actual = (root / path).read_bytes()
        assert digest(actual) == row["integration_sha256"], f"integrated digest changed: {path}"
        if row["mode"] == "exact_copy":
            assert digest(actual) == row["source_sha256"], f"source copy differs: {path}"
        else:
            offset = row["insertion_offset"]
            insertion = row["insertion_text"].encode()
            assert actual[offset : offset + len(insertion)] == insertion, f"scoped addition differs: {path}"
            projected_base = actual[:offset] + actual[offset + len(insertion) :]
            assert digest(projected_base) == row["integration_base_sha256"], f"unrelated CC bytes changed: {path}"
            assert projected_base == git(root, "show", f"{base}:{path}"), f"CC base projection differs: {path}"
            if row["mode"] == "scoped_locale_append":
                assert digest(actual[: offset + len(insertion)]) == row["source_sha256"], f"locale source delta missing: {path}"
                assert len(insertion.splitlines()) == 44, f"locale addition count differs: {path}"
                assert len(projected_base[offset:].splitlines()) == row["preserved_cc_suffix_lines"] == 108
            else:
                assert len(insertion.splitlines()) == 3, "App integration must remain the original three lines"
                assert b"clear_connection_token();" in insertion
    for path, expected in manifest["protected_integration_files"].items():
        assert digest((root / path).read_bytes()) == expected, f"excluded release file changed: {path}"
    changed = set(git(root, "diff", "--name-only", base, "--", "crates").decode().splitlines())
    changed.update(git(root, "ls-files", "--others", "--exclude-standard", "--", "crates").decode().splitlines())
    expected_paths = paths | overlay_extra_paths
    assert changed == expected_paths, f"product inventory mismatch: missing={expected_paths - changed}; extra={changed - expected_paths}"
    if args.check_source:
        source = Path(manifest["source_root"])
        assert git(source, "rev-parse", "HEAD").decode().strip() == manifest["source_head"]
        assert digest(git(source, "status", "--porcelain=v1", "-uall")) == manifest["source_status_sha256"]
        for row in files:
            assert digest((source / row["path"]).read_bytes()) == row["source_sha256"], f"original source changed: {row['path']}"
    print("PASS: 27 approved paths, missing 0; 21 exact copies; App 3-line scope; locale 44-line additions + CC108-line suffixes; release files unchanged.")
    if args.with_lifecycle:
        print("PASS: separately authorized lifecycle overlay reverses exactly to the original transfer; final product/test inventory is 28 paths.")
    if args.check_source:
        print("PASS: original source HEAD, complete Git status and 27 approved source digests remain unchanged.")
    print("No Cargo, runtime, native renderer, live app or release verification is performed by this script.")


if __name__ == "__main__":
    main()
