#!/usr/bin/env python3
"""Verify the controller or Web archive in an isolated extracted directory."""
import argparse
from pathlib import Path, PurePosixPath
import subprocess
import tarfile
import tempfile
from astra_tools.package_checks import verify_manifest
from astra_tools.platform import executable


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=Path)
    parser.add_argument('--role', choices=('controller', 'web'), required=True)
    args = parser.parse_args()
    binary = 'astra-manager' if args.role == 'controller' else 'astra-web'
    with tempfile.TemporaryDirectory(prefix=f'astra-{args.role}-package-') as temporary:
        directory = Path(temporary)
        with tarfile.open(args.archive) as archive:
            for member in archive.getmembers():
                path = PurePosixPath(member.name)
                assert not path.is_absolute() and '..' not in path.parts
                assert member.isfile() or member.isdir()
            archive.extractall(directory, filter='data')
        root, = directory.iterdir()
        assert root.name == args.role
        assert {p.name for p in root.iterdir()} == {executable(binary), 'manifest.json'}
        manifest = verify_manifest(root, args.role)
        assert set(manifest['components']) == {binary}
        subprocess.run([str(root / executable(binary)), '--help'], cwd=directory,
                       check=True, capture_output=True, timeout=10)
    print(f'PASS {args.role} archive, checksums, isolated executable help')


if __name__ == '__main__':
    main()
