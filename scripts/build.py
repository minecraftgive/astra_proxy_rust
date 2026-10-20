#!/usr/bin/env python3
"""Build, test and package the native Astra client (Windows entry: build.ps1)."""
import argparse
import json
import os
import pathlib
import platform
import re
import shutil
import subprocess
import sys
from platform_support import executable

ROOT = pathlib.Path(__file__).resolve().parents[1]


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
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--debug', action='store_true')
    parser.add_argument('--build-only', action='store_true')
    parser.add_argument('--skip-tests', action='store_true')
    parser.add_argument('--output-dir', type=pathlib.Path, default=ROOT/'dist')
    parser.add_argument('--label')
    args = parser.parse_args()
    output = args.output_dir.resolve()
    env = build_environment()

    def run(command):
        subprocess.run(list(map(str, command)), check=True, cwd=ROOT, env=env)

    def capture(command):
        return subprocess.check_output(command, cwd=ROOT, env=env, text=True)

    for tool in ('cargo', 'rustc', 'cmake'):
        if not shutil.which(tool, path=env['PATH']):
            parser.error(f'{tool} is required; see docs/INSTALL.md')
    host = next(line[6:] for line in capture(['rustc', '-vV']).splitlines() if line.startswith('host: '))
    if env.get('CARGO_BUILD_TARGET', host) != host:
        parser.error('Native packaging requires CARGO_BUILD_TARGET to match the Rust host')
    if os.name == 'nt' and platform.machine().lower() in ('amd64', 'x86_64') and not shutil.which('nasm', path=env['PATH']):
        parser.error('NASM is required by BoringSSL on Windows x64; see docs/INSTALL.md')
    metadata = json.loads(capture(['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1']))
    versions = {p['version'] for p in metadata['packages'] if p['name'] in ('astra-proxy', 'astra-rule', 'astra-tui')}
    if len(versions) != 1:
        parser.error('The three native program versions must agree')
    label = args.label or versions.pop()
    if not re.fullmatch(r'[a-zA-Z0-9.-]+', label):
        parser.error('Label must contain only letters, numbers, dots and hyphens')
    profile = ['--profile', 'dev'] if args.debug else ['--release']
    target = ['--target', host]
    binaries = pathlib.Path(metadata['target_directory']) / host / ('debug' if args.debug else 'release')
    client_packages = [item for name in ('astra-ipc','astra-rule-data','astra-rule','astra-proxy','astra-tui','astra-convert') for item in ('-p',name)]
    run(['cargo', 'build', '--locked', *client_packages, '--bins', *target, *profile])
    if os.name == 'nt':
        arch = {'amd64':'amd64', 'x86_64':'amd64', 'arm64':'arm64', 'aarch64':'arm64', 'x86':'x86', 'i686':'x86'}.get(platform.machine().lower())
        wintun = ROOT/'.build-tools/windows/wintun'/str(arch)
        if (wintun/'wintun.dll').is_file():
            for name in ('wintun.dll', 'WINTUN-LICENSE.txt', 'WINTUN-SOURCE.json'):
                shutil.copy2(wintun/name, binaries/name)
    for name in ('astra-proxy', 'astra-rule', 'astra-tui'):
        if not (binaries/executable(name)).is_file():
            raise RuntimeError(f'Missing executable: {name}')
    if not args.skip_tests:
        run(['cargo', 'test', '--locked', *client_packages, *target, *profile])
        run(['cargo', 'test', '--locked', '-p', 'astra-tui', *target, *profile, 'live_', '--', '--ignored'])
        if os.name == 'nt':
            run([sys.executable, ROOT/'scripts/windows_regression.py', '--bin-dir', binaries])
            run([sys.executable, ROOT/'scripts/ruleset_regression.py', '--bin-dir', binaries])
            run([sys.executable, ROOT/'scripts/proxy_groups_regression.py', '--bin-dir', binaries])
            run([sys.executable, ROOT/'scripts/subscription_regression.py', '--bin-dir', binaries])
    if args.build_only:
        print(f'Built client executables in {binaries}')
        return
    run([sys.executable, ROOT/'scripts/package_release.py', '--bin-dir', binaries, '--output-dir', output, '--label', label])
    archive = output/f'astra-client-{label}-{platform.system().lower()}-{platform.machine()}.tar.gz'
    if not args.skip_tests:
        run([sys.executable, ROOT/'scripts/package_regression.py', archive, '--report', ROOT/'.test-tools'/f'{archive.name}.verification.json'])
    print(f'Distribution: {archive}')


if __name__ == '__main__':
    main()
