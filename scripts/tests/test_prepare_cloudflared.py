import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'prepare-cloudflared.py'
spec = importlib.util.spec_from_file_location('prepare_cloudflared', SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def archive(payload=b'companion', link=False):
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode='w:gz') as tar:
        member = tarfile.TarInfo('cloudflared')
        if link:
            member.type = tarfile.SYMTYPE
            member.linkname = '/tmp/untrusted-helper'
            tar.addfile(member)
        else:
            member.size = len(payload)
            tar.addfile(member, io.BytesIO(payload))
    return buffer.getvalue()


class PrepareCompanionTests(unittest.TestCase):
    def test_verified_regular_binary_is_executable(self):
        data = archive()
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'deppy-cloudflared'
            module.install_archive(data, hashlib.sha256(data).hexdigest(), output)
            self.assertEqual(output.read_bytes(), b'companion')
            self.assertEqual(output.stat().st_mode & 0o777, 0o755)

    def test_wrong_digest_preserves_previous_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'deppy-cloudflared'
            output.write_bytes(b'previous')
            with self.assertRaisesRegex(ValueError, 'digest'):
                module.install_archive(archive(), '0' * 64, output)
            self.assertEqual(output.read_bytes(), b'previous')

    def test_link_member_is_not_installed(self):
        data = archive(link=True)
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'deppy-cloudflared'
            with self.assertRaisesRegex(ValueError, 'regular'):
                module.install_archive(data, hashlib.sha256(data).hexdigest(), output)
            self.assertFalse(output.exists())

    def test_both_macos_archives_are_pinned(self):
        self.assertEqual(set(module.ASSETS), {'arm64', 'x86_64'})
        for asset, digest in module.ASSETS.values():
            self.assertTrue(asset.endswith('.tgz'))
            self.assertEqual(len(digest), 64)


if __name__ == '__main__':
    unittest.main()
