#!/usr/bin/env python3
"""Build the native Astra programs; --package also creates release archives.

Building never runs tests; use scripts/test.py for that.
Entry points: build.ps1 (Windows) and build.sh (Linux/macOS) forward here.
"""
import argparse
import hashlib
import json
import os
import pathlib
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
from platform_support import executable

ROOT = pathlib.Path(__file__).resolve().parents[1]
ROLES = {
    # Built crates, archived executables, example configuration.
    'client': (
        ('astra-dns', 'astra-ipc', 'astra-rule-data', 'astra-rule', 'astra-proxy', 'astra-tui', 'astra-convert'),
        ('astra-rule', 'astra-proxy', 'astra-tui', 'astra-convert'),
        ROOT/'configs/client.example.toml',
    ),
    'server': (
        ('astra-server-core', 'astra-server-tui'),
        ('astra-server-core', 'astra-server-tui'),
        ROOT/'configs/server.example.toml',
    ),
}


def build_environment():
    env = dict(os.environ)
    local = ROOT / '.build-tools' / 'windows'
    if os.name == 'nt':
        paths = [p.parent for p in local.glob('nasm-*/nasm.exe')]
        paths += [local / 'python' / 'bin']
        env['PATH'] = os.pathsep.join(map(str, paths)) + os.pathsep + env.get('PATH', '')
        if not env.get('LIBCLANG_PATH'):
            candidates = [local / 'python/clang/native', pathlib.Path('C:/Program Files/LLVM/bin')]
            for candidate in candidates:
                if (candidate / 'libclang.dll').is_file():
                    env['LIBCLANG_PATH'] = str(candidate)
                    break
    elif not shutil.which('cmake') and (ROOT/'.build-tools/bin/cmake').is_file():
        env['PATH'] = str(ROOT/'.build-tools/bin') + os.pathsep + env.get('PATH', '')
    return env


def host_target(env):
    info = subprocess.check_output(['rustc', '-vV'], cwd=ROOT, env=env, text=True)
    return next(line[6:] for line in info.splitlines() if line.startswith('host: '))


def binary_dir(env, debug):
    """Where an explicit --target build of the host places its executables."""
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'], cwd=ROOT, env=env, text=True))
    return pathlib.Path(metadata['target_directory']) / host_target(env) / ('debug' if debug else 'release'), metadata


def package(role, bin_dir, output_dir, label):
    _, binaries, example = ROLES[role]
    names = [executable(binary) for binary in binaries]
    for name in names:
        if not (bin_dir/name).is_file():
            raise FileNotFoundError(bin_dir/name)
    # Runtime DLLs are executable code; no license, metadata or script sidecars.
    if role == 'client' and os.name == 'nt' and (bin_dir/'wintun.dll').is_file():
        names.append('wintun.dll')
    output_dir.mkdir(parents=True, exist_ok=True)
    archive = output_dir/f'astra-{role}-{label}-{platform.system().lower()}-{platform.machine()}.tar.gz'
    with tempfile.TemporaryDirectory(prefix=f'astra-{role}-package-') as temp:
        root = pathlib.Path(temp)/role
        root.mkdir()
        for name in names:
            shutil.copy2(bin_dir/name, root/name)
        shutil.copy2(example, root/f'{role}.example.toml')
        with tarfile.open(archive, 'w:gz') as tar:
            tar.add(root, arcname=role)
    print(f'Archive: {archive}\nSHA-256: {hashlib.sha256(archive.read_bytes()).hexdigest()}')
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--debug', action='store_true', help='use the debug profile (default: release)')
    parser.add_argument('--role', choices=('client', 'server', 'all'), default='client',
                        help='programs to build (default: client)')
    parser.add_argument('--package', action='store_true', help='also create one archive per role')
    parser.add_argument('--output-dir', type=pathlib.Path, default=ROOT/'dist', help='archive directory (default: dist)')
    parser.add_argument('--label', help='archive label (default: workspace package version)')
    args = parser.parse_args()
    env = build_environment()
    for tool in ('cargo', 'rustc', 'cmake'):
        if not shutil.which(tool, path=env['PATH']):
            parser.error(f'{tool} is required; see docs/INSTALL.md')
    host = host_target(env)
    if env.get('CARGO_BUILD_TARGET', host) != host:
        parser.error('Native builds require CARGO_BUILD_TARGET to match the Rust host')
    if os.name == 'nt' and platform.machine().lower() in ('amd64', 'x86_64') and not shutil.which('nasm', path=env['PATH']):
        parser.error('NASM is required by BoringSSL on Windows x64; see docs/INSTALL.md')
    bin_dir, metadata = binary_dir(env, args.debug)
    roles = ('client', 'server') if args.role == 'all' else (args.role,)
    label = args.label
    if args.package and not label:
        versions = {p['version'] for p in metadata['packages'] if p['name'] in ('astra-proxy', 'astra-rule', 'astra-tui')}
        if len(versions) != 1:
            parser.error('The three native client program versions must agree')
        label = versions.pop()
    if label and not re.fullmatch(r'[a-zA-Z0-9.-]+', label):
        parser.error('Label must contain only letters, numbers, dots and hyphens')

    crates = [item for role in roles for name in ROLES[role][0] for item in ('-p', name)]
    # Explicit --target keeps a .cargo/config.toml default from silently changing the platform.
    profile = ['--profile', 'dev'] if args.debug else ['--release']
    subprocess.run(['cargo', 'build', '--locked', *crates, '--bins', '--target', host, *profile],
                   check=True, cwd=ROOT, env=env)
    if 'client' in roles and os.name == 'nt':
        arch = {'amd64': 'amd64', 'x86_64': 'amd64', 'arm64': 'arm64', 'aarch64': 'arm64', 'x86': 'x86', 'i686': 'x86'}.get(platform.machine().lower())
        wintun = ROOT/'.build-tools/windows/wintun'/str(arch)
        if (wintun/'wintun.dll').is_file():
            for name in ('wintun.dll', 'WINTUN-LICENSE.txt', 'WINTUN-SOURCE.json'):
                shutil.copy2(wintun/name, bin_dir/name)
    for role in roles:
        for name in ROLES[role][1]:
            if not (bin_dir/executable(name)).is_file():
                raise RuntimeError(f'Missing executable: {bin_dir/executable(name)}')
    print(f'Built {", ".join(roles)} executables in {bin_dir}')
    if args.package:
        for role in roles:
            package(role, bin_dir, args.output_dir.resolve(), label)


if __name__ == '__main__':
    main()
