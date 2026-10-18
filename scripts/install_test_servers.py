#!/usr/bin/env python3
"""Install reference servers for interoperability TESTS ONLY. Not a product dependency."""
import argparse
import hashlib
import io
import json
import os
import pathlib
import platform
import tarfile
import urllib.request
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--destination', type=pathlib.Path, default=ROOT / '.test-tools' / 'bin')
    parser.add_argument('--engine', choices=['xray', 'sing-box'])
    args = parser.parse_args()
    machine = {'x86_64': 'amd64', 'aarch64': 'arm64', 'arm64': 'arm64'}.get(platform.machine())
    key = platform.system().lower() + '-' + str(machine)
    manifest = json.loads((ROOT / 'scripts' / 'reference-servers.lock.json').read_text())
    args.destination.mkdir(parents=True, exist_ok=True)
    for name, release in manifest.items():
        if args.engine and name != args.engine:
            continue
        asset = release['assets'].get(key)
        if not asset:
            raise SystemExit('No pinned engine build for ' + key)
        expected = asset['sha256']
        if not expected or not expected.startswith('sha256:'):
            raise SystemExit('Missing SHA-256 for ' + name)
        print('Downloading', name, release['version'], flush=True)
        with urllib.request.urlopen(asset['url'], timeout=120) as response:
            archive = response.read()
        if hashlib.sha256(archive).hexdigest() != expected.split(':', 1)[1]:
            raise SystemExit('SHA-256 mismatch: ' + name)
        if asset['url'].endswith('.zip'):
            with zipfile.ZipFile(io.BytesIO(archive)) as z:
                contents = z.read(name)
        else:
            with tarfile.open(fileobj=io.BytesIO(archive), mode='r:gz') as t:
                candidates = [m for m in t.getmembers() if m.isfile() and pathlib.PurePosixPath(m.name).name == name]
                if len(candidates) != 1:
                    raise SystemExit('Unexpected engine archive layout')
                contents = t.extractfile(candidates[0]).read()
        target = args.destination / name
        temporary = target.with_suffix('.download')
        temporary.write_bytes(contents)
        temporary.chmod(0o755)
        os.replace(temporary, target)
        print('Installed', target, flush=True)

if __name__ == '__main__':
    main()
