"""Restart only the currently running native Deppy app after bundle verification."""
from pathlib import Path
import datetime
import hashlib
import json
import os
import plistlib
import signal
import subprocess
import time

ROOT = Path(__file__).resolve().parents[4]
EVIDENCE = Path(__file__).resolve().parent
BUILD = json.loads((EVIDENCE / 'build-result.json').read_text())
BUNDLE = Path(BUILD['bundle'])
EXE = BUNDLE / 'Contents/MacOS/deppy-sijo'
assert BUILD['reported_version'] == 'Deppy Sijo 0.16.0'
assert BUILD['package_verification'].startswith('PASS')
assert hashlib.sha256(EXE.read_bytes()).hexdigest() == BUILD['bundle_sha256']['Contents/MacOS/deppy-sijo']
info = plistlib.loads((BUNDLE / 'Contents/Info.plist').read_bytes())
assert info['CFBundleShortVersionString'] == info['CFBundleVersion'] == '0.16.0'


def main_apps():
    rows = subprocess.check_output(['ps', '-axo', 'pid=,command='], text=True)
    result = []
    for row in rows.splitlines():
        parts = row.strip().split(None, 1)
        if len(parts) == 2 and parts[1].endswith('/Deppy Sijo.app/Contents/MacOS/deppy-sijo'):
            result.append({'pid': int(parts[0]), 'executable': parts[1]})
    return result


def still_same(app):
    return app in main_apps()


def wait_gone(app, seconds):
    end = time.monotonic() + seconds
    while still_same(app) and time.monotonic() < end:
        time.sleep(0.25)
    return not still_same(app)


previous = main_apps()
assert len(previous) <= 1, 'Multiple main apps; identify the active app before stopping anything'
record = {'authorized_request': '재빌드하고 재실행해', 'started': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'previous': previous, 'new_bundle': str(BUNDLE), 'source_commit': BUILD['source_commit'], 'command_center_mode': 'DEPPY_COMMAND_CENTER_V2=1', 'stop_actions': []}
if previous:
    old = previous[0]
    assert old['executable'] != str(EXE), 'New app already running; do not restart it twice'
    old_bundle = str(Path(old['executable']).parents[2])
    try:
        quit_result = subprocess.run(['osascript', '-e', 'tell application ' + json.dumps(old_bundle) + ' to quit'], capture_output=True, text=True, timeout=12)
        record['stop_actions'].append({'kind': 'graceful AppleEvent quit', 'exit_code': quit_result.returncode, 'stderr': quit_result.stderr.strip()})
    except subprocess.TimeoutExpired:
        record['stop_actions'].append({'kind': 'graceful AppleEvent quit', 'result': 'timeout after12s'})
    if not wait_gone(old, 8):
        assert still_same(old)
        os.kill(old['pid'], signal.SIGTERM)
        record['stop_actions'].append({'kind': 'SIGTERM exact validated PID', 'pid': old['pid']})
        if not wait_gone(old, 5):
            assert still_same(old)
            os.kill(old['pid'], signal.SIGKILL)
            record['stop_actions'].append({'kind': 'SIGKILL exact unresponsive PID after graceful/TERM deadlines', 'pid': old['pid']})
            assert wait_gone(old, 5), 'Old app did not exit'
assert not main_apps(), 'Do not launch while an old main app remains'
command = ['/usr/bin/open', '-n', '--env', 'DEPPY_COMMAND_CENTER_V2=1', str(BUNDLE)]
subprocess.run(command, check=True)
record['launch_command'] = command
end = time.monotonic() + 20
while time.monotonic() < end:
    matches = [app for app in main_apps() if app['executable'] == str(EXE)]
    if matches:
        break
    time.sleep(0.25)
assert len(matches) == 1, 'New bundle process not observed'
record['new_process'] = matches[0]
time.sleep(3)
assert still_same(matches[0]), 'New app exited immediately'
record['process_liveness'] = 'PASS after3s; UI responsiveness checked separately'
record['finished'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
(EVIDENCE / 'restart-result.json').write_text(json.dumps(record, indent=2, ensure_ascii=False) + '\n')
print(json.dumps(record, indent=2, ensure_ascii=False))
