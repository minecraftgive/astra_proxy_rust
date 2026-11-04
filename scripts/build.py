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
from astra_tools.workspace import ROOT, BuildLayout, build_environment, cargo_profile
from astra_tools.artifacts import ROLES, ArtifactIndex, role_binaries, role_packages, stage, package


FORM_FIELDS = (
    ('profile', 'Profile', (('debug', 'debug'), ('release', 'release'), ('both', 'debug + release'))),
    ('arch', 'Architecture', (('native', 'native - current Rust host'),
                              ('64bits', '64bits - require a 64-bit native target'))),
    ('role', 'Role', tuple((name, name) for name in (*ROLES, 'all'))),
    ('package', 'Package', ((False, 'no'), (True, 'yes'))),
    ('timings', 'Timing report', ((False, 'no'), (True, 'yes'))),
)


def _form_input(prompt):
    try:
        answer = input(prompt).strip().lower()
    except (EOFError, KeyboardInterrupt) as error:
        print('\nBuild cancelled.')
        raise SystemExit(130 if isinstance(error, KeyboardInterrupt) else 0) from None
    if answer in ('q', 'quit', 'cancel'):
        print('Build cancelled.')
        raise SystemExit(0)
    return answer


def interactive_options(args):
    """Editable terminal form; only an explicit Build starts compilation."""
    if not args.interactive and (not sys.stdin.isatty() or not sys.stdout.isatty()):
        return
    while True:
        print('\nAstra build form')
        print('-' * 64)
        for number, (name, label, choices) in enumerate(FORM_FIELDS, 1):
            selected = next(text for value, text in choices if value == getattr(args, name))
            print(f'  {number}. {label:14} {selected}')
        profiles = ('debug', 'release') if args.profile == 'both' else (args.profile,)
        print('  Cargo profiles: ' + ', '.join(cargo_profile(profile) for profile in profiles))
        print(f'  Output: {args.output_dir.resolve()}')
        selection = _form_input('Field number to edit | B: Build | Q: Cancel > ')
        if selection in ('b', 'build'):
            return
        if not selection:
            continue
        if selection not in {str(number) for number in range(1, len(FORM_FIELDS) + 1)}:
            print('Enter a field number, B to build, or Q to cancel.')
            continue
        name, label, choices = FORM_FIELDS[int(selection) - 1]
        print(f'\n{label}:')
        for number, (value, text) in enumerate(choices, 1):
            selected = ' [selected]' if value == getattr(args, name) else ''
            print(f'  {number}. {text}{selected}')
        while True:
            answer = _form_input('Select number/value (Enter: keep current, Q: cancel) > ')
            if not answer:
                break
            matches = [value for number, (value, text) in enumerate(choices, 1)
                       if answer in (str(number), text.split(' ')[0], str(value).lower())]
            if matches:
                setattr(args, name, matches[0])
                break
            print('Invalid choice; select one of the listed values.')


def validate_architecture(arch, target, env):
    if arch == 'native':
        return
    cfg = subprocess.check_output(['rustc', '--print', 'cfg', '--target', target],
                                  cwd=ROOT, env=env, text=True)
    if 'target_pointer_width="64"' not in cfg.splitlines():
        raise ValueError(f'64bits requires a 64-bit Rust host target; current target: {target}. '
                         'Select a 64-bit native Rust toolchain; cross-compilation is not supported.')


def build_web_assets(env):
    web = ROOT / 'apps/web'
    npm = shutil.which('npm.cmd' if os.name == 'nt' else 'npm', path=env['PATH'])
    if not npm:
        raise RuntimeError('The web role requires Node/npm; native client/server/controller builds do not')
    for arguments in (['ci'], ['run', 'typecheck'], ['run', 'build']):
        subprocess.run([npm, *arguments], cwd=web, env=env, check=True)
    if not (web / 'dist/index.html').is_file():
        raise RuntimeError('Web build did not produce apps/web/dist/index.html')


def argument_parser():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--debug', action='store_true', help='alias for --profile debug')
    parser.add_argument('--role', choices=(*ROLES, 'all'), default='client')
    parser.add_argument('--package', action='store_true')
    parser.add_argument('--output-dir', type=Path, default=ROOT / 'dist')
    parser.add_argument('--label', help='archive label (default: selected component version)')
    parser.add_argument('--profile', choices=('debug', 'release', 'both'), default='debug',
                        help='debug: Cargo dev, fastest compilation; release: Cargo release, '
                             'fastest runtime (default: debug)')
    parser.add_argument('--arch', choices=('native', '64bits'), default='native',
                        help='native Rust host, or require a 64-bit native target (x86_64/ARM64)')
    parser.add_argument('--timings', action='store_true', help='save Cargo HTML timing reports with the distribution')
    interaction = parser.add_mutually_exclusive_group()
    interaction.add_argument('--interactive', action='store_true', help='always show the build form')
    interaction.add_argument('--non-interactive', action='store_true', help='build directly without the form')
    return parser


def build_command(roles, target, profile, *, timings=False):
    crates = [item for name in role_packages(roles) for item in ('-p', name)]
    binaries = [item for name in role_binaries(roles) for item in ('--bin', name)]
    return ['cargo', 'build', '--locked', *crates, *binaries, '--target', target,
            '--profile', cargo_profile(profile), *(['--timings'] if timings else [])]


def main():
    parser = argument_parser()
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
    if env.get('CARGO_BUILD_TARGET', layout.target) not in (layout.target, 'host-tuple'):
        parser.error('Native builds require CARGO_BUILD_TARGET to match the Rust host')
    try:
        validate_architecture(args.arch, layout.target, env)
    except (ValueError, subprocess.CalledProcessError) as error:
        parser.error(str(error))
    print(f'Target: {layout.target} (architecture: {args.arch})', flush=True)
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
        selected_profile = cargo_profile(profile)
        bin_dir = layout.binary_dir(profile)
        print(f'Building {", ".join(roles)}: {profile} (Cargo profile: {selected_profile})', flush=True)
        started = time.perf_counter()
        subprocess.run(build_command(roles, layout.target, profile, timings=args.timings),
                       check=True, cwd=ROOT, env=env)
        elapsed = time.perf_counter() - started
        if args.timings:
            report = layout.target_directory / 'cargo-timings' / 'cargo-timing.html'
            if report.is_file():
                destination = run_dir / f'cargo-timing-{selected_profile}.html'
                shutil.copy2(report, destination)
                print(f'Timing report: {destination}')
        if 'client' in roles and os.name == 'nt':
            arch = {'amd64': 'amd64', 'x86_64': 'amd64', 'arm64': 'arm64', 'aarch64': 'arm64', 'x86': 'x86', 'i686': 'x86'}.get(platform.machine().lower())
            wintun = ROOT / '.build-tools/windows/wintun' / str(arch)
            for name in ('wintun.dll', 'WINTUN-LICENSE.txt', 'WINTUN-SOURCE.json'):
                if (wintun / name).is_file():
                    shutil.copy2(wintun / name, bin_dir / name)
        for role in roles:
            staged = stage(role, bin_dir, run_dir / profile / role, target=layout.target, versions=versions,
                           label=label, profile=profile)
            archive = package(role, staged, run_dir / profile, label, target=layout.target) if args.package else None
            index.add(role=role, profile=profile, binary_dir=bin_dir, staged=staged, archive=archive)
            index.save()
        print(f'Built {", ".join(roles)} in {elapsed:.2f}s: {bin_dir}')
    print(f'Distribution: {run_dir}\nArtifact index: {run_dir / "artifacts.json"}')


if __name__ == '__main__':
    main()
