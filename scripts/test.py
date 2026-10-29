#!/usr/bin/env python3
"""Run Astra tests against executables built by scripts/build.py.

Suites (default: unit live client):
  unit     cargo unit and integration tests for the whole workspace
  live     TUI end-to-end tests that drive real proxy and rule cores
  client   local client regressions (subscriptions, DNS, groups, routes, Windows)
  server   server core regressions; build first with build.py --role server
  package  boot the archives in dist/; build first with build.py --package
  release  full protocol gate; needs reference servers (install_test_servers.py)

Use the same --debug choice as the build so the tests find those executables.
"""
import argparse
import os
import pathlib
import subprocess
import sys
import time
from build import ROOT, binary_dir, build_environment, host_target

SCRIPTS = ROOT/'scripts'
CLIENT = ['subscription_diagnostics_regression', 'dns_regression', 'route_logging_regression']
# Windows hosts additionally exercise their system integration and the subscription paths.
CLIENT_WINDOWS = ['windows_regression', 'ruleset_regression', 'proxy_groups_regression',
                  'subscription_regression', 'multi_subscription_regression']
SERVER = ['server_regression', 'server_lifecycle']
SUITES = ('unit', 'live', 'client', 'server', 'package', 'release')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('suites', nargs='*', choices=SUITES, metavar='SUITE', help='suites to run')
    parser.add_argument('--debug', action='store_true', help='test the debug build (default: release)')
    parser.add_argument('--bin-dir', type=pathlib.Path, help='executables to test (default: build.py output)')
    parser.add_argument('--keep-going', action='store_true', help='run every step and report all failures')
    args = parser.parse_args()
    suites = args.suites or ['unit', 'live', 'client']
    env = build_environment()
    host = host_target(env)
    bin_dir = (args.bin_dir or binary_dir(env, args.debug)[0]).resolve()
    cargo = ['--locked', '--target', host, *(['--profile', 'dev'] if args.debug else ['--release'])]
    python = [sys.executable]

    steps = []
    if 'unit' in suites:
        steps.append(('unit', ['cargo', 'test', '--workspace', *cargo]))
    if 'live' in suites:
        steps.append(('live', ['cargo', 'test', '-p', 'astra-tui', *cargo, 'live_', '--', '--ignored']))
    if 'client' in suites:
        for name in CLIENT + (CLIENT_WINDOWS if os.name == 'nt' else []):
            steps.append((name, python + [SCRIPTS/f'{name}.py', '--bin-dir', bin_dir]))
    if 'server' in suites:
        for name in SERVER:
            steps.append((name, python + [SCRIPTS/f'{name}.py', '--bin-dir', bin_dir]))
    if 'package' in suites:
        archives = sorted((ROOT/'dist').glob('astra-*.tar.gz'))
        if not archives:
            parser.error('no archives in dist/; run build.py --package first')
        for archive in archives:
            if archive.name.startswith('astra-client-'):
                report = ROOT/'.test-tools'/f'{archive.name}.verification.json'
                steps.append((archive.name, python + [SCRIPTS/'package_regression.py', archive, '--report', report]))
            elif archive.name.startswith('astra-server-'):
                steps.append((archive.name, python + [SCRIPTS/'server_package_regression.py', archive]))
    if 'release' in suites:
        steps.append(('release', python + [SCRIPTS/'release_check.py', '--bin-dir', bin_dir]))
    if any(s in suites for s in ('client', 'server', 'release')) and not bin_dir.is_dir():
        parser.error(f'{bin_dir} does not exist; build first with scripts/build.py')

    failed = []
    for name, command in steps:
        print(f'\n== {name}', flush=True)
        start = time.monotonic()
        code = subprocess.run(list(map(str, command)), cwd=ROOT, env=env).returncode
        status = 'PASS' if code == 0 else f'FAIL ({code})'
        print(f'== {status} {name} · {time.monotonic() - start:.1f}s', flush=True)
        if code:
            failed.append(name)
            if not args.keep_going:
                break
    print()
    if failed:
        raise SystemExit(f'Failed: {", ".join(failed)}')
    print(f'All {len(steps)} test steps passed')


if __name__ == '__main__':
    main()
