#!/usr/bin/env python3
"""Refresh only the managed helper pair used by raw developer executables."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import tempfile

SPEC = importlib.util.spec_from_file_location("prepare_remote_host", Path(__file__).with_name("prepare-remote-host.py"))
HELPERS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPERS)


def verify_managed(directory):
    HELPERS.require(not directory.is_symlink() and directory.is_dir(), "helper_dev_destination_invalid")
    HELPERS.require({path.name for path in directory.iterdir()} == set(HELPERS.TARGETS) | {"manifest.json"}, "helper_dev_destination_unmanaged")
    for label in HELPERS.TARGETS:
        target = directory / label
        HELPERS.require(not target.is_symlink() and target.is_dir(), "helper_dev_destination_invalid")
        HELPERS.require({path.name for path in target.iterdir()} == {"deppy-remote-host"}, "helper_dev_destination_unmanaged")
    descriptor = HELPERS.open_directory(directory)
    try:
        entries = json.loads(HELPERS.read_fixed(descriptor, "manifest.json", HELPERS.MANIFEST_MAX), object_pairs_hook=HELPERS.unique_keys)
    finally:
        os.close(descriptor)
    HELPERS.require(isinstance(entries, list) and len(entries) == 2 and isinstance(entries[0], dict), "helper_manifest_targets_invalid")
    # An older managed version may be replaced, but corrupted or foreign content may not.
    HELPERS.prepare(directory, None, entries[0].get("version", ""))


def refresh(source, version):
    output = Path("target/Resources/remote-host")
    HELPERS.require(not output.parent.is_symlink() and not output.is_symlink(), "helper_dev_destination_invalid")
    output.parent.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix=".deppy-dev-helpers-", dir=output.parent))
    removable = True
    try:
        replacement = stage / "replacement"
        HELPERS.prepare(source, replacement, version)
        previous = stage / "previous"
        if output.exists():
            verify_managed(output)
            output.rename(previous)
            removable = False
        try:
            replacement.rename(output)
        except OSError:
            if previous.exists():
                previous.rename(output)
                removable = True
            raise
        removable = True
    finally:
        # If rollback itself fails, preserve the previous resources for recovery.
        if removable:
            shutil.rmtree(stage)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--version", required=True)
    arguments = parser.parse_args()
    try:
        refresh(arguments.source, arguments.version)
    except (ValueError, OSError) as error:
        code = str(error) if isinstance(error, ValueError) else "helper_dev_resource_io_failed"
        parser.exit(1, "developer helper validation failed: " + code + "\n")


if __name__ == "__main__":
    main()
