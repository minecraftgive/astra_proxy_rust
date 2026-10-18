#!/usr/bin/env python3
"""Package the client executables and one comprehensive example configuration."""
import argparse
import hashlib
import os
import pathlib
import platform
import shutil
import tarfile
import tempfile
from protocol_matrix import ROOT
from platform_support import executable


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def package(role, binaries, example, args):
    if not args.label or any(c not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-' for c in args.label):
        raise ValueError('invalid label')
    names = [executable(binary) for binary in binaries]
    for name in names:
        if not (args.bin_dir/name).is_file():
            raise FileNotFoundError(args.bin_dir/name)
    # Runtime DLLs are executable code; no license, metadata or script sidecars.
    if role == 'client' and os.name == 'nt' and (args.bin_dir/'wintun.dll').is_file():
        names.append('wintun.dll')
    name = f'astra-{role}-{args.label}-{platform.system().lower()}-{platform.machine()}'
    args.output_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=f'astra-{role}-package-') as temp:
        root = pathlib.Path(temp)/role
        root.mkdir()
        for binary in names:
            shutil.copy2(args.bin_dir/binary, root/binary)
        shutil.copy2(example, root/f'{role}.example.toml')
        archive = args.output_dir/(name+'.tar.gz')
        with tarfile.open(archive, 'w:gz') as tar:
            tar.add(root, arcname=role)
        print(archive.resolve())
        print(sha(archive))
    return archive


def arguments(description):
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT/'target/release')
    parser.add_argument('--output-dir', type=pathlib.Path, default=ROOT/'dist')
    parser.add_argument('--label', default='0.1.0')
    return parser.parse_args()


def main():
    args = arguments(__doc__)
    package('client', ('astra-rule','astra-proxy','astra-tui','astra-convert'), ROOT/'configs/client.example.toml', args)


if __name__ == '__main__':
    main()
