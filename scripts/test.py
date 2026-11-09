#!/usr/bin/env python3
"""Run Astra tests against executables built by scripts/build.py.

Suites (default: unit live client):
  check    Rust workspace and all target compilation checks
  unit     cargo unit and integration tests for the whole workspace
  live     TUI and manager end-to-end tests that drive real proxy and rule cores
  client   local client regressions (subscriptions, DNS, groups, routes, Windows)
  server   server core regressions; build first with build.py --role server
  management  real TLS Controller/Agents/Web; --artifact-dir validates all four packages
  package  boot the archives in dist/; build first with build.py --package
  release  full protocol gate; needs reference servers (install_test_servers.py)

Use the same --debug choice as the build (Cargo profile dev; default release),
or pass --artifact-dir to test a build run from scripts/build.py.
"""
import argparse
import os
import pathlib
import sys
from astra_tools.workspace import ROOT, resolve_binary_dir, build_environment, host_target, cargo_profile
from astra_tools.artifacts import ROLES, ArtifactIndex, discover_index
from astra_tools.runner import Step, run_steps, timestamped_report

SCRIPTS = ROOT/'scripts'
CLIENT = ['subscription_diagnostics_regression', 'dns_regression', 'route_logging_regression',
          'ruleset_regression', 'proxy_groups_regression', 'subscription_regression',
          'multi_subscription_regression']
# This fixture specifically covers Windows IPC and long/Unicode state paths.
CLIENT_WINDOWS = ['windows_regression']
SERVER = ['server_regression', 'server_lifecycle']
SUITES = ('tools', 'architecture', 'check', 'unit', 'live', 'client', 'server', 'management', 'web', 'package', 'release')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('suites', nargs='*', choices=SUITES, metavar='SUITE', help='suites to run')
    parser.add_argument('--debug', action='store_true', help='test the debug build (default: release)')
    parser.add_argument('--bin-dir', type=pathlib.Path, help='executables to test (default: build.py output)')
    parser.add_argument('--artifact-dir', type=pathlib.Path, help='build run directory containing artifacts.json')
    parser.add_argument('--role', choices=tuple(ROLES), help='restrict artifact/package selection')
    parser.add_argument('--report', type=pathlib.Path, help='write suite results as JSON')
    parser.add_argument('--keep-going', action='store_true', help='run every step and report all failures')
    parser.add_argument('--browser', action='store_true', help='include Playwright in the management suite')
    args = parser.parse_args()
    suites = args.suites or ['unit', 'live', 'client']
    if args.browser and 'management' not in suites:
        parser.error('--browser requires the management suite')
    env = dict(os.environ) if set(suites) <= {'tools', 'architecture', 'package', 'web'} else build_environment()
    profile = 'debug' if args.debug else 'release'
    needs_bins = any(s in suites for s in ('unit', 'live', 'client', 'server', 'release')) or ('management' in suites and not args.artifact_dir)
    try:
        index = None
        artifact_dir = args.artifact_dir
        if 'package' in suites:
            index = ArtifactIndex.load(artifact_dir) if artifact_dir else discover_index()
            artifact_dir = index.directory
        bin_dir = resolve_binary_dir(args.bin_dir, artifact_dir=artifact_dir, profile=profile,
                                     role=args.role, env=env) if needs_bins else None
    except (ValueError, OSError) as error:
        parser.error(str(error))
    if any(suite in suites for suite in ('unit', 'live')):
        env['ASTRA_UPDATE_TEST_BIN'] = str(bin_dir)
    host = host_target(env) if any(s in suites for s in ('check', 'unit', 'live', 'web')) else None
    cargo = ['--locked', '--target', host, '--profile', cargo_profile(profile)]
    python = [sys.executable]

    steps = []
    if 'tools' in suites or 'unit' in suites:
        steps.append(('tools', python + ['-m', 'unittest', 'discover', '-s', 'scripts/tests', '-v']))
    if 'architecture' in suites:
        steps.append(('architecture', python + [SCRIPTS/'check_architecture.py']))
    if 'check' in suites:
        steps.append(('check', ['cargo', 'check', '--workspace', '--all-targets', *cargo]))
    if 'unit' in suites:
        steps.append(('unit', ['cargo', 'test', '--workspace', '--no-fail-fast', *cargo]))
    if 'live' in suites:
        steps.append(('live', ['cargo', 'test', '-p', 'astra-tui', *cargo, 'live_', '--', '--ignored']))
        steps.append(('manager live', ['cargo', 'test', '-p', 'astra-manager', *cargo, 'live_', '--', '--ignored']))
    if 'client' in suites:
        for name in CLIENT + (CLIENT_WINDOWS if os.name == 'nt' else []):
            steps.append((name, python + [SCRIPTS/f'{name}.py', '--bin-dir', bin_dir]))
    if 'server' in suites:
        for name in SERVER:
            steps.append((name, python + [SCRIPTS/f'{name}.py', '--bin-dir', bin_dir]))
    if 'management' in suites:
        source = ['--artifact-dir', artifact_dir] if artifact_dir else ['--bin-dir', bin_dir]
        steps.append(Step('management', python + [SCRIPTS/'management_regression.py', *source,
                          '--profile', profile, '--require-cores', '--report', ROOT/'.test-tools/management-scenario.json',
                          *(['--browser'] if args.browser else [])], timeout=600))
    if 'package' in suites:
        try:
            archives = index.archives(profile=profile, role=args.role)
        except (ValueError, OSError) as error:
            parser.error(str(error))
        for role, archive in archives:
            if role == 'client':
                report = ROOT/'.test-tools'/f'{archive.name}.verification.json'
                steps.append((archive.name, python + [SCRIPTS/'package_regression.py', archive, '--report', report]))
            elif role == 'server':
                steps.append((archive.name, python + [SCRIPTS/'server_package_regression.py', archive]))
            else:
                steps.append((archive.name, python + [SCRIPTS/'service_package_regression.py', archive, '--role', role]))
    if 'web' in suites:
        # A codegen tool: always the fast-compiling dev profile.
        contract_flags = ['--locked', '--target', host, '--profile', 'dev']
        steps.append(('contracts', ['cargo', 'run', *contract_flags, '-p', 'astra-management', '--features', 'bindings', '--bin', 'export-contracts', '--', '--check']))
        npm = 'npm.cmd' if os.name == 'nt' else 'npm'
        for script in ('test', 'build'):
            steps.append(Step(f'web {script}', [npm, 'run', script], cwd=ROOT/'apps/web'))
    if 'release' in suites:
        steps.append(('release', python + [SCRIPTS/'release_check.py', '--bin-dir', bin_dir, '--output', timestamped_report('release')]))
    if needs_bins and not bin_dir.is_dir():
        parser.error(f'{bin_dir} does not exist; build first with scripts/build.py')

    results = run_steps([step if isinstance(step, Step) else Step(*step) for step in steps],
                        env=env, keep_going=args.keep_going, report=args.report)
    failed = [result['name'] for result in results if result['exit_code']]
    print()
    if failed:
        raise SystemExit(f'Failed: {", ".join(failed)}')
    print(f'All {len(steps)} test steps passed')


if __name__ == '__main__':
    main()
