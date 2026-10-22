"""Verify trust before executing a harmless companion; never launch the app."""
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
VERSION = re.search(r'^version\s*=\s*"([^"]+)"', (ROOT / "Cargo.toml").read_text(), re.MULTILINE).group(1)


@unittest.skipUnless(sys.platform == "darwin", "requires actual macOS code signing")
class PackageTrust(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="deppy-package-trust-")
        self.addCleanup(self.temporary.cleanup)
        self.fixture = Path(self.temporary.name)
        self.bundle = self.fixture / "Deppy Sijo.app"
        macos = self.bundle / "Contents/MacOS"
        resources = self.bundle / "Contents/Resources"
        macos.mkdir(parents=True)
        resources.mkdir()
        source = self.fixture / "companion.c"
        source.write_text(
            '#include <stdio.h>\n#include <stdlib.h>\n'
            'int main(void) { const char *p=getenv("DEPPY_TEST_COMPANION_MARKER"); '
            'if(p){FILE *f=fopen(p,"w");if(f){fputs("executed\\n",f);fclose(f);}} '
            'puts("deppy-ssh-askpass ' + VERSION + '"); return 0;}\n'
        )
        binary = self.fixture / "companion"
        subprocess.run(["/usr/bin/clang", str(source), "-o", str(binary)], check=True, capture_output=True)
        for name in ["deppy-sijo", "deppy-mcp-proxy", "deppy-ssh-askpass", "deppy-cloudflared"]:
            destination = macos / name
            shutil.copy2(binary, destination)
            subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(destination)], check=True, capture_output=True)
        metadata = {
            "CFBundlePackageType": "APPL",
            "CFBundleIdentifier": "app.vector9.deppy-sijo",
            "CFBundleExecutable": "deppy-sijo",
            "CFBundleVersion": VERSION,
            "CFBundleShortVersionString": VERSION,
            "LSMinimumSystemVersion": "11.0",
            "DeppySshWorkspaces": False,
        }
        (self.bundle / "Contents/Info.plist").write_bytes(plistlib.dumps(metadata))
        (resources / "cloudflared-LICENSE").write_text("fixture notice\n")
        subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(self.bundle)], check=True, capture_output=True)
        self.marker = self.fixture / "companion-executed"

    def verify(self, trusted):
        environment = dict(
            os.environ,
            DEPPY_REQUIRE_TRUSTED_SIGNING="1" if trusted else "0",
            DEPPY_ALLOW_UNTRUSTED_SIGNING="0" if trusted else "1",
            DEPPY_PACKAGE_SSH_WORKSPACES="0",
            DEPPY_REQUIRED_ARCHS=os.uname().machine,
            DEPPY_TEST_COMPANION_MARKER=str(self.marker),
        )
        return subprocess.run(
            ["sh", "scripts/verify-macos-package.sh", str(self.bundle)],
            cwd=ROOT, env=environment, capture_output=True, text=True, timeout=15,
        )

    def test_production_rejects_adhoc_bundle_before_executing_companion(self):
        result = self.verify(trusted=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("trusted TeamIdentifier is invalid", result.stderr)
        self.assertFalse(self.marker.exists(), "a rejected bundle executed its untrusted companion")

    def test_explicit_development_optin_still_checks_companion_version(self):
        result = self.verify(trusted=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker.read_text(), "executed\n")


if __name__ == "__main__":
    unittest.main()
