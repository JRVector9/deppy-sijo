#!/usr/bin/env python3
"""Verify final integrated source, captured execution logs, and original-ref preservation.

This verifies existing evidence. It does not run Cargo, the app, or live-device tests.
"""
import gzip
import hashlib
import json
import subprocess
from pathlib import Path


def sha(data):
    return hashlib.sha256(data).hexdigest()


def git(root, *args):
    return subprocess.check_output(['git', *args], cwd=root).decode().strip()


def main():
    evidence = Path(__file__).resolve().parent
    root = evidence.parents[3]
    manifest = json.loads((evidence / 'final-source-manifest.json').read_text())
    files = manifest['files']
    expected = {row['path'] for row in files}
    assert len(files) == len(expected)
    paths = set(git(root, 'ls-files', '--', 'Cargo.toml', 'Cargo.lock', 'crates', 'scripts', 'web', 'xtask', 'third_party').splitlines())
    paths.update(git(root, 'ls-files', '--others', '--exclude-standard', '--', 'crates', 'scripts', 'web', 'xtask', 'third_party').splitlines())
    assert expected == paths, f'source inventory differs: missing={expected - paths}, extra={paths - expected}'
    for row in files:
        assert sha((root / row['path']).read_bytes()) == row['sha256'], row['path']
    executions = {}
    for path in sorted(evidence.glob('*.json')):
        execution = json.loads(path.read_text())
        if not isinstance(execution, dict) or not {'label', 'argv', 'log_sha256'} <= execution.keys():
            continue
        label = execution['label']
        readable = evidence / (label + '.log')
        archive = evidence / execution.get('raw_log_gzip', label + '.log.gz')
        raw = gzip.decompress(archive.read_bytes()) if archive.exists() else readable.read_bytes()
        assert sha(raw) == execution['log_sha256'], f'raw execution log: {label}'
        if 'readable_log_sha256' in execution:
            assert sha(readable.read_bytes()) == execution['readable_log_sha256'], label
        executions[label] = execution['exit']
    final = json.loads((evidence / 'final-gates.json').read_text())
    for label in final['required_green_labels']:
        assert executions.get(label) == 0, f'required gate did not pass: {label}'
    assert final['source_manifest_sha256'] == sha((evidence / 'final-source-manifest.json').read_bytes())
    # Original ref baseline excludes the four newly created integration/worker branches.
    now = {line.split(' ', 1)[1]: line.split(' ', 1)[0] for line in git(root, 'show-ref').splitlines()}
    originals = json.loads((evidence / 'original-ref-baseline.json').read_text())
    for ref, commit in originals.items():
        assert now.get(ref) == commit, f'original ref moved: {ref}'
    preservation = json.loads((evidence / 'original-preservation-final.json').read_text())
    assert preservation['original_refs']['count'] == len(originals) == 301
    assert preservation['original_refs']['changed'] == []
    assert all(row['head_unchanged'] and not row['changed_files'] for row in preservation['sources'].values())
    print(f'PASS: {len(files)} integrated source digests, {len(executions)} raw execution logs, {len(final["required_green_labels"])} final gates, {len(originals)} original refs.')
    print('Original source file preservation is recorded by original-preservation-final.json; no tests or app execution performed by this verifier.')


if __name__ == '__main__':
    main()
