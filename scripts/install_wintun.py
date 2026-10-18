#!/usr/bin/env python3
"""Prepare signed Wintun DLLs and their license; does not load/install the driver."""
import argparse
import hashlib
import io
import json
import pathlib
import platform
import shutil
import urllib.request
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
VERSION = '0.14.1'
URL = f'https://www.wintun.net/builds/wintun-{VERSION}.zip'
SHA256 = '07c256185d6ee3652e09fa55c0b673e2624b565e02c4b9091c79ca7d2f24ef51'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', type=pathlib.Path, help='use a previously downloaded official ZIP')
    parser.add_argument('--bin-dir', type=pathlib.Path, help='also copy files beside a built astra-proxy.exe')
    args = parser.parse_args()
    if args.archive:
        data = args.archive.read_bytes()
    else:
        with urllib.request.urlopen(URL, timeout=60) as response:
            data = response.read()
    if hashlib.sha256(data).hexdigest() != SHA256:
        raise SystemExit('Wintun archive SHA-256 mismatch; nothing extracted')
    destination = ROOT/'.build-tools/windows/wintun'
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        license_data = archive.read('wintun/LICENSE.txt')
        libraries = {arch: archive.read(f'wintun/bin/{arch}/wintun.dll') for arch in ('amd64', 'x86', 'arm64', 'arm')}
        for arch in ('amd64', 'x86', 'arm64', 'arm'):
            directory = destination/arch
            directory.mkdir(parents=True, exist_ok=True)
            (directory/'wintun.dll').write_bytes(libraries[arch])
            (directory/'WINTUN-LICENSE.txt').write_bytes(license_data)
            (directory/'WINTUN-SOURCE.json').write_text(json.dumps({'version': VERSION, 'url': URL, 'archive_sha256': SHA256, 'architecture': arch}, indent=2)+'\n', encoding='utf-8')
    if args.bin_dir:
        arch = {'amd64':'amd64', 'x86_64':'amd64', 'arm64':'arm64', 'aarch64':'arm64', 'x86':'x86', 'i686':'x86'}.get(platform.machine().lower())
        if not arch:
            parser.error('Unknown native architecture; copy the matching DLL manually')
        args.bin_dir.mkdir(parents=True, exist_ok=True)
        for name in ('wintun.dll', 'WINTUN-LICENSE.txt', 'WINTUN-SOURCE.json'):
            shutil.copy2(destination/arch/name, args.bin_dir/name)
    print(f'Prepared Wintun {VERSION} at {destination} (driver not loaded)')


if __name__ == '__main__':
    main()
