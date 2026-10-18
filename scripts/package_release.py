#!/usr/bin/env python3
"""Bundle local native binaries, examples, documentation and dependency notices."""
import argparse
import hashlib
import json
import os
import pathlib
import platform
import shutil
import subprocess
import tarfile
import tempfile
from protocol_matrix import ROOT


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def notices(destination):
    metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version', '1', '--filter-platform', subprocess.check_output(['rustc', '-vV'], text=True).split('host: ')[1].splitlines()[0]], cwd=ROOT, text=True))
    packages = {p['id']: p for p in metadata['packages']}
    nodes = {n['id']: n for n in metadata['resolve']['nodes']}
    todo, seen = list(metadata['workspace_members']), set()
    while todo:
        node = todo.pop()
        if node in seen:
            continue
        seen.add(node)
        todo.extend(dep['pkg'] for dep in nodes[node]['deps'] if any(k['kind'] != 'dev' for k in dep['dep_kinds']))
    index = []
    for key in sorted(seen):
        package = packages[key]
        source = pathlib.Path(package['manifest_path']).parent
        if key in metadata['workspace_members']:
            continue
        folder = destination / f"{package['name']}-{package['version']}"
        files = set()
        if package.get('license_file'):
            files.add(source / package['license_file'])
        # Include bundled crypto-library notices as well as crate-level licenses.
        for path in source.rglob('*'):
            if path.is_file() and path.name.upper().startswith(('LICENSE', 'LICENCE', 'COPYING', 'NOTICE')) and not {'target', '.git'}.intersection(path.relative_to(source).parts):
                files.add(path)
        copied = []
        for path in sorted(files):
            if not path.is_file():
                continue
            relative = path.relative_to(source)
            target = folder / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
            copied.append(str(relative))
        if not copied:
            fallback = ROOT/'vendor/license-notices'/f"{package['name']}-{package['version']}"
            if fallback.is_dir():
                folder.mkdir(parents=True, exist_ok=True)
                for notice in fallback.iterdir():
                    shutil.copy2(notice, folder/notice.name)
                    copied.append(notice.name)
            elif package['repository'] == 'https://github.com/meow-rs/meow-rs' and package['version'] == '0.23.1':
                folder.mkdir(parents=True, exist_ok=True)
                shutil.copy2(ROOT/'vendor/meow-proxy/LICENSE', folder/'LICENSE')
                (folder/'SOURCE.txt').write_text('Shared meow-rs repository MIT license at v0.23.1; source https://github.com/meow-rs/meow-rs/blob/v0.23.1/LICENSE\n')
                copied = ['LICENSE', 'SOURCE.txt']
            else:
                raise RuntimeError(f"No license notice for {package['name']} {package['version']}")
        index.append({'name': package['name'], 'version': package['version'], 'license': package.get('license'), 'repository': package.get('repository'), 'notice_files': copied})
    destination.mkdir(parents=True, exist_ok=True)
    (destination / 'index.json').write_text(json.dumps(index, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT/'target/release')
    parser.add_argument('--output-dir', type=pathlib.Path, default=ROOT/'dist')
    parser.add_argument('--label', default='0.1.0')
    args = parser.parse_args()
    if not args.label or any(c not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-' for c in args.label):
        parser.error('label must contain only letters, numbers, dots and hyphens')
    binaries = args.bin_dir.resolve()
    for binary in ('astra-rule', 'astra-proxy', 'astra-tui'):
        path = binaries/binary
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f'missing executable {path}; run scripts/build.sh to build all three programs')
    name = f'astra-{args.label}-{platform.system().lower()}-{platform.machine()}'
    args.output_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='astra-package-') as td:
        root = pathlib.Path(td)/name
        (root/'bin').mkdir(parents=True)
        for binary in ('astra-rule', 'astra-proxy', 'astra-tui'):
            shutil.copy2(binaries/binary, root/'bin'/binary)
        for directory in ('configs', 'docs'):
            shutil.copytree(ROOT/directory, root/directory)
        shutil.copy2(ROOT/'README.md', root/'README.md')
        shutil.copy2(ROOT/'Cargo.lock', root/'Cargo.lock')
        (root/'BUILD_INFO.json').write_text(json.dumps({'rustc': subprocess.check_output(['rustc','-vV'], text=True), 'cargo_lock_sha256': sha(ROOT/'Cargo.lock'), 'platform': platform.platform()}, indent=2)+'\n')
        (root/'scripts').mkdir()
        shutil.copy2(ROOT/'scripts/astra.py', root/'scripts/astra.py')
        notices(root/'third-party-notices')
        (root/'vendor-patches').mkdir()
        shutil.copy2(ROOT/'vendor/meow-proxy/ASTRA_PATCHES.md', root/'vendor-patches/ASTRA_PATCHES.md')
        manifest = {'label': args.label, 'platform': platform.platform(),
                    'files': {str(p.relative_to(root)): sha(p) for p in sorted(root.rglob('*')) if p.is_file()}}
        (root/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
        archive = args.output_dir/(name+'.tar.gz')
        with tarfile.open(archive, 'w:gz') as tar:
            tar.add(root, arcname=name)
        checksum = sha(archive)
        archive.with_suffix(archive.suffix+'.sha256').write_text(f'{checksum}  {archive.name}\n')
        print(archive.resolve())
        print(checksum)


if __name__ == '__main__':
    main()
