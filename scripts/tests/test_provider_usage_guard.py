"""Mutations happen only inside a temporary synthetic repository."""
import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('provider_usage_guard', Path(__file__).resolve().parents[1] / 'verify-provider-usage.py')
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)

class ProviderUsageGuardTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.path = self.root / 'usage.rs'
        self.original = 'unrelated before\n// BEGIN\napproved behavior\n// END\nunrelated after\n'
        self.path.write_text(self.original)

    def test_whole_file_change_is_rejected(self):
        contract = {'protected': [{'path': 'usage.rs', 'sha256': hashlib.sha256(self.original.encode()).hexdigest()}]}
        self.assertEqual(GUARD.check_contract(self.root, contract), [])
        self.path.write_text(self.original.replace('approved', 'broken'))
        self.assertTrue(GUARD.check_contract(self.root, contract))

    def test_only_approved_region_is_frozen(self):
        contract = {'protected': [{'path': 'usage.rs', 'start': '// BEGIN\n', 'end': '// END', 'sha256': hashlib.sha256(b'approved behavior\n').hexdigest()}]}
        self.assertEqual(GUARD.check_contract(self.root, contract), [])
        self.path.write_text(self.original.replace('unrelated before', 'a new unrelated feature'))
        self.assertEqual(GUARD.check_contract(self.root, contract), [])
        self.path.write_text(self.original.replace('approved', 'broken'))
        self.assertTrue(GUARD.check_contract(self.root, contract))

    def test_missing_and_duplicated_markers_are_rejected(self):
        contract = {'protected': [{'path': 'usage.rs', 'start': '// BEGIN\n', 'end': '// END', 'sha256': hashlib.sha256(b'approved behavior\n').hexdigest()}]}
        for changed in [self.original.replace('// END', '// removed'), self.original + '// BEGIN\n', self.original + '// END']:
            self.path.write_text(changed)
            self.assertTrue(GUARD.check_contract(self.root, contract))

    def test_missing_protected_file_is_rejected(self):
        self.assertTrue(GUARD.check_contract(self.root, {'protected': [{'path': 'missing.rs', 'sha256': '0' * 64}]}))

    def test_removed_or_ignored_mandatory_test_is_rejected(self):
        expected = ['tests::provider_usage_harness_example']
        self.assertEqual(GUARD.check_inventory(expected, expected, []), [])
        self.assertTrue(GUARD.check_inventory(expected, [], []))
        self.assertTrue(GUARD.check_inventory(expected, expected, expected))
        self.assertTrue(GUARD.check_inventory(expected, expected + ['unexpected'], []))

if __name__ == '__main__':
    unittest.main()
