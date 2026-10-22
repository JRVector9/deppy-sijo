import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("prepare_remote_host", Path(__file__).resolve().parents[1] / "prepare-remote-host.py")
HELPERS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HELPERS)
TARGETS = {"linux-aarch64": 183, "linux-x86_64": 62}

def elf(machine, interpreter=False):
    binary = bytearray(256)
    binary[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HHIQQQIHHHHHH", binary, 16, 2, machine, 1, 0x400000, 64, 0, 0, 64, 56, 1, 0, 0, 0)
    struct.pack_into("<IIQQQQQQ", binary, 64, 3 if interpreter else 1, 5, 0, 0x400000, 0x400000, 256, 256, 4096)
    return bytes(binary)

class HelperArtifacts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="deppy-helper-package-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.output = self.root / "output"
        self.manifest = []
        for label, machine in TARGETS.items():
            directory = self.source / label
            directory.mkdir()
            binary = elf(machine)
            (directory / "deppy-remote-host").write_bytes(binary)
            self.manifest.append(dict(version="0.14.0", target=label, sha256=hashlib.sha256(binary).hexdigest(), bytes=len(binary)))
        self.write_manifest()

    def write_manifest(self):
        (self.source / "manifest.json").write_text(json.dumps(self.manifest))

    def test_copies_both_verified_static_architectures_with_exact_manifest(self):
        actual = HELPERS.prepare(self.source, self.output, "0.14.0")
        self.assertEqual(actual, self.manifest)
        self.assertEqual(json.loads((self.output / "manifest.json").read_text()), self.manifest)
        for label in TARGETS:
            self.assertEqual((self.source / label / "deppy-remote-host").read_bytes(), (self.output / label / "deppy-remote-host").read_bytes())

    def test_rejects_tampering_wrong_size_version_architecture_and_dynamic_linking(self):
        for mutation in ["digest", "bytes", "version", "architecture", "interpreter", "needed"]:
            with self.subTest(mutation=mutation):
                binary = bytearray(elf(183))
                self.manifest[0] = dict(version="0.14.0", target="linux-aarch64", sha256=hashlib.sha256(binary).hexdigest(), bytes=len(binary))
                if mutation == "digest": binary[-1] = 1
                if mutation == "bytes": self.manifest[0]["bytes"] += 1
                if mutation == "version": self.manifest[0]["version"] = "0.13.0"
                if mutation == "architecture": struct.pack_into("<H", binary, 18, 62)
                if mutation == "interpreter": binary = bytearray(elf(183, True))
                if mutation == "needed":
                    binary.extend(bytes(128))
                    struct.pack_into("<H", binary, 56, 2)
                    struct.pack_into("<IIQQQQQQ", binary, 120, 2, 6, 256, 0, 0, 32, 32, 8)
                    struct.pack_into("<QQQQ", binary, 256, 1, 1, 0, 0)
                if mutation in ["architecture", "interpreter", "needed"]:
                    self.manifest[0]["sha256"] = hashlib.sha256(binary).hexdigest()
                    self.manifest[0]["bytes"] = len(binary)
                (self.source / "linux-aarch64/deppy-remote-host").write_bytes(binary)
                self.write_manifest()
                with self.assertRaises(ValueError) as raised: HELPERS.prepare(self.source, None, "0.14.0")
                if mutation == "needed": self.assertEqual(str(raised.exception), "helper_dynamic_dependency_forbidden")

    def test_manifest_cannot_supply_paths_duplicate_targets_unknown_keys_or_unbounded_data(self):
        original = json.loads(json.dumps(self.manifest))
        for mutation in ["path", "duplicate", "unknown", "oversize", "missing"]:
            with self.subTest(mutation=mutation):
                self.manifest = json.loads(json.dumps(original))
                if mutation == "path": self.manifest[0]["target"] = "../outside"
                if mutation == "duplicate": self.manifest[1] = self.manifest[0]
                if mutation == "unknown": self.manifest[0]["path"] = "../../outside"
                if mutation == "missing": self.manifest.pop()
                self.write_manifest()
                if mutation == "oversize": (self.source / "manifest.json").write_bytes(b" " * (65536 + 1))
                with self.assertRaises(ValueError): HELPERS.prepare(self.source, None, "0.14.0")

    def test_rejects_symlink_directories_and_files_without_following_them(self):
        binary = self.source / "linux-aarch64/deppy-remote-host"
        outside = self.root / "outside"
        binary.rename(outside)
        binary.symlink_to(outside)
        with self.assertRaises(ValueError): HELPERS.prepare(self.source, None, "0.14.0")
        binary.unlink()
        outside.rename(binary)
        directory = self.source / "linux-aarch64"
        directory.rename(self.root / "outside-directory")
        directory.symlink_to(self.root / "outside-directory", target_is_directory=True)
        with self.assertRaises(ValueError): HELPERS.prepare(self.source, None, "0.14.0")

    def test_generate_manifest_is_explicit_and_never_overwrites_output(self):
        (self.source / "manifest.json").unlink()
        actual = HELPERS.prepare(self.source, self.output, "0.14.0", create_manifest=True)
        self.assertEqual(actual, self.manifest)
        with self.assertRaises(ValueError): HELPERS.prepare(self.source, self.output, "0.14.0", create_manifest=True)

if __name__ == "__main__": unittest.main()
