#!/usr/bin/env python3
"""Validate a fixed pair of static Linux helpers before sealing app resources.

This never executes foreign binaries. build-remote-host.sh checks --version in
Linux before generating the manifest; release copy verifies those sealed bytes.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import struct
import tempfile

TARGETS = {"linux-aarch64": 183, "linux-x86_64": 62}
BINARY_MAX = 128 * 1024 * 1024
MANIFEST_MAX = 64 * 1024


def require(condition, code):
    if not condition:
        raise ValueError(code)


def read_fixed(directory, name, limit):
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK
    try:
        descriptor = os.open(name, flags, dir_fd=directory)
    except OSError as error:
        raise ValueError("helper_resource_not_regular") from error
    with os.fdopen(descriptor, "rb") as stream:
        before = os.fstat(stream.fileno())
        require(stat.S_ISREG(before.st_mode) and before.st_size <= limit, "helper_resource_size_invalid")
        binary = stream.read(limit + 1)
        after = os.fstat(stream.fileno())
        require(len(binary) <= limit and len(binary) == before.st_size, "helper_resource_size_invalid")
        require((before.st_size, before.st_mtime_ns, before.st_ctime_ns) == (after.st_size, after.st_mtime_ns, after.st_ctime_ns), "helper_resource_changed")
        return binary


def open_directory(path, parent=None):
    try:
        return os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent)
    except OSError as error:
        raise ValueError("helper_directory_invalid") from error


def verify_elf(binary, machine):
    require(len(binary) >= 64 and binary[:7] == b"\x7fELF\x02\x01\x01" and binary[7] in (0, 3), "helper_elf_invalid")
    header = struct.unpack_from("<HHIQQQIHHHHHH", binary, 16)
    elf_type, actual_machine, elf_version = header[:3]
    offset, header_size, entry_size, count = header[4], header[7], header[8], header[9]
    require(elf_type in (2, 3) and actual_machine == machine and elf_version == 1, "helper_architecture_invalid")
    require(header_size == 64 and entry_size == 56 and 1 <= count <= 256 and offset >= 64 and offset + count * entry_size <= len(binary), "helper_program_headers_invalid")
    loadable = False
    for index in range(count):
        kind, _, position, _, _, size, _, _ = struct.unpack_from("<IIQQQQQQ", binary, offset + index * entry_size)
        require(position + size <= len(binary), "helper_segment_invalid")
        require(kind != 3, "helper_dynamic_interpreter_forbidden")
        if kind == 1:
            loadable = True
        if kind == 2:
            require(size % 16 == 0, "helper_dynamic_segment_invalid")
            for cursor in range(position, position + size, 16):
                tag, _ = struct.unpack_from("<QQ", binary, cursor)
                if tag == 0:
                    break
                require(tag not in (1, 15, 29), "helper_dynamic_dependency_forbidden")
    require(loadable, "helper_load_segment_missing")


def unique_keys(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "helper_manifest_duplicate_key")
        result[key] = value
    return result


def prepare(source, output, version, *, create_manifest=False):
    require(re.fullmatch(r"[0-9]{1,8}\.[0-9]{1,8}\.[0-9]{1,8}", version) is not None, "helper_version_invalid")
    directory = open_directory(source)
    try:
        binaries = {}
        for label, machine in TARGETS.items():
            target = open_directory(label, directory)
            try:
                binary = read_fixed(target, "deppy-remote-host", BINARY_MAX)
            finally:
                os.close(target)
            verify_elf(binary, machine)
            binaries[label] = binary
        if create_manifest:
            entries = [dict(version=version, target=label, sha256=hashlib.sha256(binary).hexdigest(), bytes=len(binary)) for label, binary in binaries.items()]
        else:
            try:
                entries = json.loads(read_fixed(directory, "manifest.json", MANIFEST_MAX), object_pairs_hook=unique_keys)
            except (json.JSONDecodeError, UnicodeDecodeError) as error:
                raise ValueError("helper_manifest_invalid") from error
        require(isinstance(entries, list) and len(entries) == 2, "helper_manifest_targets_invalid")
        found = set()
        for entry in entries:
            require(isinstance(entry, dict) and set(entry) == {"version", "target", "sha256", "bytes"}, "helper_manifest_fields_invalid")
            label = entry["target"]
            require(isinstance(label, str) and label in TARGETS and label not in found, "helper_manifest_targets_invalid")
            found.add(label)
            require(entry["version"] == version, "helper_version_mismatch")
            require(type(entry["bytes"]) is int and entry["bytes"] == len(binaries[label]), "helper_size_mismatch")
            require(isinstance(entry["sha256"], str) and re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]) is not None and entry["sha256"] == hashlib.sha256(binaries[label]).hexdigest(), "helper_digest_mismatch")
        require(found == set(TARGETS), "helper_manifest_targets_invalid")
    finally:
        os.close(directory)
    if output is not None:
        output = Path(output)
        require(not output.exists() and not output.is_symlink(), "helper_output_already_exists")
        output.parent.mkdir(parents=True, exist_ok=True)
        stage = Path(tempfile.mkdtemp(prefix=".deppy-helper-stage-", dir=output.parent))
        try:
            for label, binary in binaries.items():
                (stage / label).mkdir(mode=0o755)
                destination = stage / label / "deppy-remote-host"
                destination.write_bytes(binary)
                destination.chmod(0o755)
            (stage / "manifest.json").write_text(json.dumps(entries, indent=2) + "\n")
            stage.chmod(0o755)
            # Publish immutable bytes, refusing an existing nonempty destination.
            require(not output.exists() and not output.is_symlink(), "helper_output_already_exists")
            stage.rename(output)
        finally:
            if stage.exists():
                shutil.rmtree(stage)
    return entries


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--create-manifest", action="store_true", help="only after Linux --version checks in the build script")
    arguments = parser.parse_args()
    try:
        prepare(arguments.source, arguments.output, arguments.version, create_manifest=arguments.create_manifest)
    except (ValueError, OSError) as error:
        code = str(error) if isinstance(error, ValueError) else "helper_resource_io_failed"
        parser.exit(1, "remote helper validation failed: " + code + "\n")


if __name__ == "__main__":
    main()
