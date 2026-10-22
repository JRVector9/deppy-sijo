"""Exercise packaging argument routing without builds, signing, or bundle writes."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]

class SshPackaging(unittest.TestCase):
    def run_package(self, arguments):
        with tempfile.TemporaryDirectory(prefix="deppy-package-routing-") as temp:
            directory = Path(temp)
            log = directory / "cargo-arguments"
            for name, body in {"security": "exit 0", "cargo": "printf '%s\\n' \"$@\" > \"$DEPPY_TEST_CARGO_LOG\"\nexit 97"}.items():
                binary = directory / name
                binary.write_text("#!/bin/sh\n" + body + "\n")
                binary.chmod(0o755)
            environment = dict(os.environ, PATH=str(directory) + os.pathsep + os.environ["PATH"], DEPPY_REQUIRE_TRUSTED_SIGNING="0", DEPPY_ALLOW_UNTRUSTED_SIGNING="1", DEPPY_TEST_CARGO_LOG=str(log))
            result = subprocess.run(["sh", "scripts/package-macos.sh", *arguments], cwd=ROOT, env=environment, capture_output=True)
            return result, log.read_text().splitlines() if log.exists() else []

    def test_ssh_feature_is_explicit_and_forwarded_to_cargo(self):
        result, args = self.run_package(["--features", "ssh-workspaces"])
        self.assertEqual(result.returncode, 97)
        self.assertIn("--locked", args)
        self.assertIn("deppy-sijo/ssh-workspaces", args)
        result, args = self.run_package([])
        self.assertEqual(result.returncode, 97)
        self.assertNotIn("deppy-sijo/ssh-workspaces", args)

    def test_default_features_cannot_implicitly_enable_ssh(self):
        for arguments in [[], ["--features", "ssh-workspaces"]]:
            with self.subTest(arguments=arguments):
                result, args = self.run_package(arguments)
                self.assertEqual(result.returncode, 97)
                self.assertIn("--no-default-features", args)

    def test_unknown_or_incomplete_feature_arguments_stop_before_cargo(self):
        for args in [["--features"], ["--features", "anything"], ["--anything"], ["--features", "ssh-workspaces", "extra"]]:
            with self.subTest(args=args):
                result, cargo = self.run_package(args)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(cargo, [])

if __name__ == "__main__": unittest.main()
