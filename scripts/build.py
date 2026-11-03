#!/usr/bin/env python3
"""Build native programs and optionally package client/server/controller/web roles.

No tests run during builds. Ordinary native roles do not require Node.
"""
import argparse
from datetime import datetime
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import time
from astra_tools.workspace import ROOT, BuildLayout, build_environment
from astra_tools.artifacts import ROLES, ArtifactIndex, role_packages, stage, package
from astra_tools.platform import executable

def interactive_options(args):
    """Single-key menu; every key restarts the inactivity deadline."""
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        return
    import contextlib
    @contextlib.contextmanager
    def keyboard():
        if os.name == 'nt':
            import msvcrt
            yield lambda: msvcrt.getwch() if msvcrt.kbhit() else None
        else:
            import select
            import termios
            import tty
            old = termios.tcgetattr(sys.stdin)
            try:
                tty.setcbreak(sys.stdin.fileno())
                yield lambda: sys.stdin.read(1) if select.select([sys.stdin], [], [], 0)[0] else None
            finally:
                termios.tcsetattr(sys.stdin, termios.TCSADRAIN, old)
    deadline = time.monotonic() + 10
    previous = None
    with keyboard() as read_key:
        while True:
            remaining = max(0, int(deadline - time.monotonic() + 0.999))
            status = (args.profile, args.role, args.package, remaining)
            if status != previous:
                print(f"\r[P] Profile: {args.profile:7}  [R] Role: {args.role:6}  "
                      f"[A] Archive: {'yes' if args.package else 'no ':3}  "
                      f"[Enter] Build  [Q] Quit  Idle: {remaining:2}s   ", end='', flush=True)
                previous = status
            if time.monotonic() >= deadline:
                print()
                return
            key = read_key()
            if key is None:
                time.sleep(0.05)
                continue
            deadline = time.monotonic() + 10
            key = key.lower()
            if key in ('q', '\x03', '\x1b'):
                print()
                raise SystemExit(0)
            if key in ('\r', '\n'):
                print()
                return
            if key == 'p':
                values = ('both', 'debug', 'release')
                args.profile = values[(values.index(args.profile) + 1) % len(values)]
            elif key == 'r':
                values = ('client', 'server', 'controller', 'web', 'all')
                args.role = values[(values.index(args.role) + 1) % len(values)]
            elif key == 'a':
                args.package = not args.package


def build_web_assets(env):
    web = ROOT / 'apps/web'
    npm = shutil.which('npm.cmd' if os.name == 'nt' else 'npm', path=env['PATH'])
    if not npm:
        raise RuntimeError('The web role requires Node/npm; native client/server/controller builds do not')
    for arguments in (['ci'], ['run', 'typecheck'], ['run', 'build']):
        subprocess.run([npm, *arguments], cwd=web, env=env, check=True)
    if not (web / 'dist/index.html').is_file():
        raise RuntimeError('Web build did not produce apps/web/dist/index.html')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--debug', action='store_true', help='alias for --profile debug')
    parser.add_argument('--role', choices=(*ROLES, 'all'), default='client')
    parser.add_argument('--package', action='store_true')
    parser.add_argument('--output-dir', type=Path, default=ROOT / 'dist')
    parser.add_argument('--label', help='archive label (default: selected component version)')
    parser.add_argument('--profile', choices=('debug', 'release', 'both'), default='both')
    parser.add_argument('--non-interactive', action='store_true')
    args = parser.parse_args()
    if args.debug:
        args.profile = 'debug'
    if not args.non_interactive:
        interactive_options(args)
    env = build_environment()
    for tool in ('cargo', 'rustc', 'cmake'):
        if not shutil.which(tool, path=env['PATH']):
            parser.error(f'{tool} is required; see docs/INSTALL.md')
    layout, metadata = BuildLayout.discover(env)
    if env.get('CARGO_BUILD_TARGET', layout.target) != layout.target:
        parser.error('Native builds require CARGO_BUILD_TARGET to match the Rust host')
    if os.name == 'nt' and platform.machine().lower() in ('amd64', 'x86_64') and not shutil.which('nasm', path=env['PATH']):
        parser.error('NASM is required by BoringSSL on Windows x64; see docs/INSTALL.md')
    roles = tuple(ROLES) if args.role == 'all' else (args.role,)
    packages = role_packages(roles)
    versions = {p['name']: p['version'] for p in metadata['packages']}
    missing = set(packages) - versions.keys()
    if missing:
        parser.error(f'Missing workspace packages: {", ".join(sorted(missing))}')
    selected_versions = {versions[p] for p in packages}
    if not args.label and len(selected_versions) != 1:
        parser.error('Selected component versions differ; provide --label')
    label = args.label or next(iter(selected_versions))
    if not re.fullmatch(r'[a-zA-Z0-9.-]+', label):
        parser.error('Label must contain only letters, numbers, dots and hyphens')
    if 'web' in roles:
        build_web_assets(env)
        env['ASTRA_WEB_REQUIRE_ASSETS'] = '1'
    run_dir = args.output_dir.resolve() / datetime.now().astimezone().strftime('%Y-%m-%d_%H-%M-%S_%f')
    run_dir.mkdir(parents=True, exist_ok=False)
    revision = subprocess.run(['git', 'rev-parse', 'HEAD'], cwd=ROOT, capture_output=True, text=True)
    index = ArtifactIndex(run_dir, layout.target, revision.stdout.strip() if revision.returncode == 0 else '')
    profiles = ('debug', 'release') if args.profile == 'both' else (args.profile,)
    for profile in profiles:
        bin_dir = layout.binary_dir(profile)
        flags = ['--profile', 'dev'] if profile == 'debug' else ['--release']
        crates = [item for name in packages for item in ('-p', name)]
        subprocess.run(['cargo', 'build', '--locked', *crates, '--bins', '--target', layout.target, *flags],
                       check=True, cwd=ROOT, env=env)
        if 'client' in roles and os.name == 'nt':
            arch = {'amd64': 'amd64', 'x86_64': 'amd64', 'arm64': 'arm64', 'aarch64': 'arm64', 'x86': 'x86', 'i686': 'x86'}.get(platform.machine().lower())
            wintun = ROOT / '.build-tools/windows/wintun' / str(arch)
            for name in ('wintun.dll', 'WINTUN-LICENSE.txt', 'WINTUN-SOURCE.json'):
                if (wintun / name).is_file():
                    shutil.copy2(wintun / name, bin_dir / name)
        for role in roles:
            staged = stage(role, bin_dir, run_dir / profile / role, target=layout.target, versions=versions, label=label)
            archive = package(role, staged, run_dir / profile, label, target=layout.target) if args.package else None
            index.add(role=role, profile=profile, binary_dir=bin_dir, staged=staged, archive=archive)
            index.save()
        print(f'Built {", ".join(roles)} in {bin_dir}')
    print(f'Distribution: {run_dir}\nArtifact index: {run_dir / "artifacts.json"}')


if __name__ == '__main__':
    main()
