"""Check developer build commands/assets without running the app-launching script."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

from scripts.tests.test_prepare_remote_host import elf

ROOT = Path(__file__).resolve().parents[2]


class DevBuildArguments(unittest.TestCase):
    def build_arguments(self, release):
        # Execute only the real build command, never dev-run.sh or its App exec.
        build_script = ROOT / "scripts/dev-build.sh"
        source = (build_script if build_script.exists() else ROOT / "scripts/dev-run.sh").read_text()
        commands = [line.strip() for line in source.splitlines() if line.strip().startswith("cargo build ")]
        self.assertEqual(len(commands), 1, "the developer path must have one direct build command")
        with tempfile.TemporaryDirectory(prefix="deppy-dev-build-argv-") as temp:
            directory = Path(temp)
            cargo = directory / "cargo"
            cargo.write_text('#!/bin/sh\nprintf \'%s\\n\' "$@" > "$DEPPY_TEST_CARGO_LOG"\nexit 97\n')
            cargo.chmod(0o755)
            log = directory / "arguments"
            environment = dict(os.environ, PATH=str(directory) + os.pathsep + os.environ["PATH"], DEPPY_TEST_CARGO_LOG=str(log), CARGO_ARGS="--release" if release else "")
            result = subprocess.run(["sh", "-c", commands[0]], cwd=ROOT, env=environment, capture_output=True)
            self.assertEqual(result.returncode, 97)
            arguments = log.read_text().splitlines()
            self.assertEqual("--release" in arguments, release)
            return arguments

    def test_debug_and_release_build_the_askpass_companion(self):
        for release in [False, True]:
            with self.subTest(release=release):
                arguments = self.build_arguments(release)
                packages = [arguments[index + 1] for index, arg in enumerate(arguments[:-1]) if arg == "-p"]
                self.assertIn("deppy-sijo", packages)
                self.assertIn("mcp-proxy", packages)
                self.assertIn("ssh-askpass", packages)

    def test_debug_and_release_use_the_locked_dependency_graph(self):
        for release in [False, True]:
            with self.subTest(release=release):
                self.assertIn("--locked", self.build_arguments(release))


class DevBuildResources(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="deppy-dev-build-resources-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "artifacts"
        self.source.mkdir()
        self.resources = self.root / "target/Resources/remote-host"
        self.version = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.MULTILINE).group(1)
        self.manifest = []
        for label, machine in {"linux-aarch64": 183, "linux-x86_64": 62}.items():
            directory = self.source / label
            directory.mkdir()
            binary = elf(machine)
            (directory / "deppy-remote-host").write_bytes(binary)
            self.manifest.append(dict(version=self.version, target=label, sha256=hashlib.sha256(binary).hexdigest(), bytes=len(binary)))
        self.write_manifest()
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "' + self.version + '"\n')
        scripts = self.root / "scripts"
        scripts.mkdir()
        for name in ["prepare-remote-host.py", "prepare-dev-resources.py"]:
            if (ROOT / "scripts" / name).exists():
                shutil.copyfile(ROOT / "scripts" / name, scripts / name)
        build_script = ROOT / "scripts/dev-build.sh"
        if build_script.exists():
            shutil.copyfile(build_script, scripts / "dev-build.sh")
        else:
            # Before extraction, exercise only the existing actual build/assets
            # commands in this isolated tree. No signing or App-launch code is copied.
            lines = (ROOT / "scripts/dev-run.sh").read_text().splitlines()
            start = next(index for index, line in enumerate(lines) if line.startswith("cargo build "))
            end = next(index for index in range(start, len(lines)) if lines[index].startswith("# 서명 우선순위"))
            prelude = '#!/bin/sh\nset -eu\ncd "$(dirname "$0")/.."\nPROFILE_DIR=debug\nCARGO_ARGS=""\nif [ "${1:-}" = "--release" ]; then PROFILE_DIR=release; CARGO_ARGS=--release; fi\n'
            (scripts / "dev-build.sh").write_text(prelude + "\n".join(lines[start:end]) + "\n")
        # Avoid network, actual compilation, codesigning and App execution.
        (scripts / "prepare-cloudflared.py").write_text('import pathlib,sys\np=pathlib.Path(sys.argv[sys.argv.index("--output")+1]);p.parent.mkdir(parents=True,exist_ok=True);p.write_text("fixture tunnel")\n')
        tools = self.root / "tools"
        tools.mkdir()
        cargo = tools / "cargo"
        cargo.write_text('#!/usr/bin/env python3\nimport os,pathlib,sys\nprofile="release" if "--release" in sys.argv else "debug"\np=pathlib.Path("target")/profile;p.mkdir(parents=True,exist_ok=True)\nfor name in ["deppy-sijo","deppy-mcp-proxy","deppy-ssh-askpass"]:\n f=p/name;f.write_text(\'#!/bin/sh\\nprintf executed > "$DEPPY_TEST_APP_EXECUTED"\\n\');f.chmod(0o755)\n')
        cargo.chmod(0o755)
        security = tools / "security"
        security.write_text("#!/bin/sh\nexit 0\n")
        security.chmod(0o755)
        self.environment = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ["PATH"], DEPPY_SIGN_IDENTITY="", DEPPY_REMOTE_HOST_ARTIFACTS=str(self.source), DEPPY_TEST_APP_EXECUTED=str(self.root / "app-executed"))

    def write_manifest(self):
        (self.source / "manifest.json").write_text(json.dumps(self.manifest))

    def build(self, release=False):
        result = subprocess.run(["sh", "scripts/dev-build.sh", *( ["--release"] if release else [])], cwd=self.root, env=self.environment, capture_output=True)
        self.assertFalse((self.root / "app-executed").exists(), "the build-only path executed a companion or App")
        return result

    def snapshot(self, directory):
        return {str(path.relative_to(directory)): path.read_bytes() for path in directory.rglob("*") if path.is_file()}

    def assert_staged_pair(self):
        actual = self.snapshot(self.resources)
        expected = self.snapshot(self.source)
        self.assertEqual(set(actual), set(expected))
        self.assertEqual(json.loads(actual.pop("manifest.json")), json.loads(expected.pop("manifest.json")))
        self.assertEqual(actual, expected)

    def test_debug_and_release_stage_exact_pair_at_raw_executable_resource_path(self):
        for release in [False, True]:
            with self.subTest(release=release):
                result = self.build(release)
                self.assertEqual(result.returncode, 0, result.stderr.decode())
                self.assert_staged_pair()

    def test_missing_helpers_fail_during_build(self):
        shutil.rmtree(self.source)
        result = self.build()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"scripts/build-remote-host.sh", result.stderr)
        self.assertIn(b"DEPPY_REMOTE_HOST_ARTIFACTS", result.stderr)
        self.assertFalse(self.resources.exists())

    def test_version_mismatch_fails_during_build(self):
        self.manifest[0]["version"] = "999.999.999"
        self.write_manifest()
        self.assertNotEqual(self.build().returncode, 0)
        self.assertFalse(self.resources.exists())

    def test_symlink_source_binary_is_rejected(self):
        binary = self.source / "linux-aarch64/deppy-remote-host"
        outside = self.root / "outside-binary"
        binary.rename(outside)
        binary.symlink_to(outside)
        self.assertNotEqual(self.build().returncode, 0)
        self.assertFalse(self.resources.exists())

    def test_symlink_resource_destination_is_rejected_without_touching_target(self):
        outside = self.root / "outside-resources"
        outside.mkdir()
        (outside / "keep").write_text("unchanged")
        self.resources.parent.mkdir(parents=True)
        self.resources.symlink_to(outside, target_is_directory=True)
        self.assertNotEqual(self.build().returncode, 0)
        self.assertEqual((outside / "keep").read_text(), "unchanged")
        self.assertEqual(list(outside.iterdir()), [outside / "keep"])

    def test_refresh_refuses_unmanaged_resources(self):
        shutil.copytree(self.source, self.resources)
        (self.resources / "keep").write_text("unmanaged")
        previous = self.snapshot(self.resources)
        self.assertNotEqual(self.build().returncode, 0)
        self.assertEqual(self.snapshot(self.resources), previous)

    def test_validated_refresh_replaces_previous_managed_bytes(self):
        shutil.copytree(self.source, self.resources)
        binary = self.source / "linux-aarch64/deppy-remote-host"
        updated = binary.read_bytes()[:-1] + b"x"
        binary.write_bytes(updated)
        self.manifest[0]["sha256"] = hashlib.sha256(updated).hexdigest()
        self.write_manifest()
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assert_staged_pair()

    def test_failed_validation_preserves_previous_managed_resources(self):
        shutil.copytree(self.source, self.resources)
        previous = self.snapshot(self.resources)
        binary = self.source / "linux-aarch64/deppy-remote-host"
        binary.write_bytes(binary.read_bytes()[:-1] + b"x")
        self.assertNotEqual(self.build().returncode, 0)
        self.assertEqual(self.snapshot(self.resources), previous)


if __name__ == "__main__":
    unittest.main()
