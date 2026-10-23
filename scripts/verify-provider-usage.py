#!/usr/bin/env python3
"""Reject unapproved provider-usage edits and run the mandatory behavioral contract."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = Path('docs/testing/provider-usage-contract.json')


def protected_bytes(root, entry):
    path = (root / entry['path']).resolve()
    if not path.is_relative_to(root.resolve()):
        raise ValueError('protected path escapes the repository')
    data = path.read_bytes()
    if 'start' in entry or 'end' in entry:
        start, end = entry['start'].encode(), entry['end'].encode()
        if data.count(start) != 1 or data.count(end) != 1:
            raise ValueError('protected markers are missing or duplicated')
        before, body = data.split(start, 1)
        del before
        if end not in body:
            raise ValueError('protected end precedes its start')
        data = body.split(end, 1)[0]
    return data


def check_contract(root, contract):
    errors = []
    entries = contract.get('protected', [])
    if not entries:
        return ['protected source inventory is empty']
    for entry in entries:
        try:
            actual = hashlib.sha256(protected_bytes(root, entry)).hexdigest()
            if actual != entry['sha256']:
                errors.append(f"{entry['path']}: approved usage contract changed")
        except (OSError, ValueError, KeyError) as error:
            errors.append(f"{entry.get('path', '?')}: {error}")
    return errors


def check_inventory(expected, listed, ignored):
    errors = []
    if not expected or len(expected) != len(set(expected)):
        errors.append('mandatory test inventory is empty or duplicated')
    if set(expected) != set(listed) or len(listed) != len(set(listed)):
        errors.append('mandatory provider usage test inventory changed')
    if set(expected) & set(ignored):
        errors.append('a mandatory provider usage test was ignored')
    return errors


def cargo_run(command):
    result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if result.returncode:
        print(result.stdout, end='')
        raise RuntimeError('provider usage Cargo check failed')
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--check-only', action='store_true')
    mode.add_argument('--seal', action='store_true', help='explicit authorized contract update; never an automatic repair')
    parser.add_argument('--authorization', help='reference to explicit user authorization, required to reseal')
    parser.add_argument('--offline', action='store_true')
    args = parser.parse_args()
    contract = json.loads((ROOT / MANIFEST).read_text())
    if contract.get('schema') != 1 or not contract.get('test_targets'):
        parser.error('contract schema or mandatory test targets are missing')
    if args.seal:
        if not args.authorization or not args.authorization.strip():
            parser.error('--seal requires --authorization with the explicit user request')
        for entry in contract['protected']:
            entry['sha256'] = hashlib.sha256(protected_bytes(ROOT, entry)).hexdigest()
        contract['authorization'] = args.authorization.strip()
        (ROOT / MANIFEST).write_text(json.dumps(contract, ensure_ascii=False, indent=2) + '\n')
        print('Authorized provider usage contract sealed; full verification is still required.')
        return 0
    errors = check_contract(ROOT, contract)
    if errors:
        print('\n'.join(errors), file=sys.stderr)
        print('Usage is protected. Do not reseal without an explicit request to change this behavior.', file=sys.stderr)
        return 1
    print('Provider usage protected source: PASS', flush=True)
    if args.check_only:
        return 0
    for target in contract['test_targets']:
        command = ['cargo', 'test', '--locked']
        if args.offline:
            command.append('--offline')
        command += ['-p', target['package'], '--bin', target['bin'], 'provider_usage_harness', '--']
        listed = re.findall(r'^(.+): test$', cargo_run(command + ['--list']), re.M)
        ignored = re.findall(r'^(.+): test$', cargo_run(command + ['--list', '--ignored']), re.M)
        errors = check_inventory(target['tests'], listed, ignored)
        if errors:
            print('\n'.join(errors), file=sys.stderr)
            return 1
        output = cargo_run(command + ['--test-threads=4'])
        summary = re.search(r'test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored', output)
        if not summary or tuple(map(int, summary.groups())) != (len(target['tests']), 0, 0):
            print(output, end='')
            print('Mandatory provider usage tests did not all execute.', file=sys.stderr)
            return 1
        print(f"Provider usage harness ({target['package']}): {len(target['tests'])} PASS / 0 ignored", flush=True)
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, RuntimeError) as error:
        print(f'Provider usage verification failed: {error}', file=sys.stderr)
        sys.exit(1)
