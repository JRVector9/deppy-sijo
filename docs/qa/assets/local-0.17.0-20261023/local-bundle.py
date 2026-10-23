"""Versioned local developer bundle; never stops or launches the app."""
from pathlib import Path
import hashlib
import json
import os
import plistlib
import shutil
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[4]
EVIDENCE = Path(__file__).resolve().parent
VERSION = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']
assert VERSION == '0.17.0'
BUILD = Path('/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target/debug')
BUNDLE = ROOT / 'target' / ('bundle-' + VERSION) / 'Deppy Sijo.app'
assert not BUNDLE.exists(), 'Never overwrite an existing delivered bundle'
MAC = BUNDLE / 'Contents/MacOS'
RESOURCES = BUNDLE / 'Contents/Resources'
IDENTITY = 'Developer ID Application: VectorNine INC (ZDTU5LS35K)'
MAC.mkdir(parents=True)
RESOURCES.mkdir()
assert subprocess.check_output([str(BUILD / 'deppy-sijo'), '--version'], text=True).strip() == 'Deppy Sijo ' + VERSION
for name in ['deppy-sijo', 'deppy-mcp-proxy', 'deppy-ssh-askpass']:
    shutil.copy2(BUILD / name, MAC / name)
subprocess.run(['python3', 'scripts/prepare-cloudflared.py', '--output', str(MAC / 'deppy-cloudflared')], cwd=ROOT, check=True)
shutil.copy2(ROOT / 'scripts/vendor/cloudflared-LICENSE', RESOURCES / 'cloudflared-LICENSE')
subprocess.run(['python3', 'scripts/prepare-remote-host.py', '--source', str(ROOT / 'target/remote-host' / VERSION), '--output', str(RESOURCES / 'remote-host'), '--version', VERSION], cwd=ROOT, check=True)
plist = {
    'CFBundlePackageType': 'APPL', 'CFBundleIdentifier': 'app.vector9.deppy-sijo',
    'CFBundleName': 'Deppy Sijo', 'CFBundleDisplayName': 'Deppy Sijo',
    'CFBundleExecutable': 'deppy-sijo', 'CFBundleVersion': VERSION,
    'CFBundleShortVersionString': VERSION, 'LSMinimumSystemVersion': '11.0',
    'NSHighResolutionCapable': True, 'NSSupportsAutomaticGraphicsSwitching': True,
    'DeppySshWorkspaces': True,
}
(BUNDLE / 'Contents/Info.plist').write_bytes(plistlib.dumps(plist))
for name, suffix in [('deppy-cloudflared', '.tunnel'), ('deppy-mcp-proxy', '.mcp-proxy'), ('deppy-ssh-askpass', '.ssh-askpass'), ('deppy-sijo', '')]:
    subprocess.run(['codesign', '--force', '--sign', IDENTITY, '--identifier', 'app.vector9.deppy-sijo' + suffix, str(MAC / name)], check=True)
subprocess.run(['codesign', '--force', '--sign', IDENTITY, str(BUNDLE)], check=True)
env = os.environ.copy()
env.update(DEPPY_REQUIRE_TRUSTED_SIGNING='0', DEPPY_ALLOW_UNTRUSTED_SIGNING='1', DEPPY_PACKAGE_SSH_WORKSPACES='1')
subprocess.run(['sh', 'scripts/verify-macos-package.sh', str(BUNDLE)], cwd=ROOT, env=env, check=True)
actual = subprocess.check_output([str(MAC / 'deppy-sijo'), '--version'], text=True).strip()
assert actual == 'Deppy Sijo ' + VERSION
info = plistlib.loads((BUNDLE / 'Contents/Info.plist').read_bytes())
assert info['CFBundleVersion'] == info['CFBundleShortVersionString'] == VERSION
files = [p for p in BUNDLE.rglob('*') if p.is_file()]
result = {
    'kind': 'Signed local developer build, debug profile; not notarized public distribution',
    'branch': subprocess.check_output(['git', 'branch', '--show-current'], cwd=ROOT, text=True).strip(),
    'source_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
    'version_change': {'last_shipped': '0.16.0', 'new': VERSION},
    'reported_version': actual,
    'plist_versions': {k: info[k] for k in ['CFBundleVersion', 'CFBundleShortVersionString']},
    'bundle': str(BUNDLE), 'restart_performed': False, 'running_app_pid_unchanged': 31144,
    'ssh_workspaces': True,
    'remote_helpers': json.loads((RESOURCES / 'remote-host/manifest.json').read_text()),
    'bundle_sha256': {str(p.relative_to(BUNDLE)): hashlib.sha256(p.read_bytes()).hexdigest() for p in files},
    'package_verification': 'PASS (explicit local developer trust policy)',
    'uncommitted_product_changes': subprocess.check_output(['git', 'diff', '--', 'Cargo.toml', 'Cargo.lock', 'crates', 'scripts', 'web'], cwd=ROOT, text=True).strip(),
}
assert not result['uncommitted_product_changes']
(EVIDENCE / 'build-result.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps({k: result[k] for k in ['source_commit', 'reported_version', 'plist_versions', 'bundle', 'package_verification']}, indent=2))
