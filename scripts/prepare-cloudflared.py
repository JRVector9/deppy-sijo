#!/usr/bin/env python3
"""Prepare the pinned MCP tunnel companion; end users need no global install."""
import argparse
import hashlib
import io
from pathlib import Path
import platform
import tarfile
import tempfile
import urllib.request

VERSION = '2026.9.1'
ASSETS = {
    'arm64': ('cloudflared-darwin-arm64.tgz', 'c27ab8fd0aa489449e3d201eb02f957ef460a13b613662928b1b23394bf1bcfe'),
    'x86_64': ('cloudflared-darwin-amd64.tgz', 'ff0d3b51d5ff70eceef89d6b32145fee985018a2174596a5dbe405e2766e2ac4'),
}
MAX_ARCHIVE = 64 * 1024 * 1024
MAX_BINARY = 128 * 1024 * 1024


def install_archive(data, expected_digest, output):
    if len(data) > MAX_ARCHIVE or hashlib.sha256(data).hexdigest() != expected_digest:
        raise ValueError('cloudflared archive digest mismatch')
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:gz') as archive:
        members = archive.getmembers()
        matches = [member for member in members if member.name in ('cloudflared', './cloudflared')]
        if len(matches) != 1 or not matches[0].isfile():
            raise ValueError('cloudflared must be a single regular archive member')
        member = matches[0]
        if not 0 < member.size <= MAX_BINARY:
            raise ValueError('cloudflared binary size invalid')
        with archive.extractfile(member) as stream:
            binary = stream.read(MAX_BINARY + 1)
        if len(binary) != member.size:
            raise ValueError('cloudflared binary truncated')
    output = Path(output)
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=output.parent, prefix='.cloudflared-', delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(binary)
        temporary.chmod(0o755)
        temporary.replace(output)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if platform.system() != 'Darwin' or platform.machine() not in ASSETS:
        parser.error('bundled automatic tunnel currently supports macOS arm64/x86_64')
    asset, digest = ASSETS[platform.machine()]
    root = Path(__file__).resolve().parents[1]
    cache = root / 'target/tools/cloudflared' / VERSION / asset
    data = cache.read_bytes() if cache.is_file() else b''
    if hashlib.sha256(data).hexdigest() != digest:
        url = f'https://github.com/cloudflare/cloudflared/releases/download/{VERSION}/{asset}'
        request = urllib.request.Request(url, headers={'User-Agent': 'Deppy-build'})
        with urllib.request.urlopen(request, timeout=60) as response:
            data = response.read(MAX_ARCHIVE + 1)
        if len(data) > MAX_ARCHIVE or hashlib.sha256(data).hexdigest() != digest:
            raise ValueError('downloaded cloudflared archive digest mismatch')
        cache.parent.mkdir(parents=True, exist_ok=True)
        cache.write_bytes(data)
    install_archive(data, digest, args.output)
    print(f'Prepared verified cloudflared {VERSION}: {args.output}')


if __name__ == '__main__':
    main()
