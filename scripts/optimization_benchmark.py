#!/usr/bin/env python3
"""Alternate immutable baseline/current binaries through the existing loopback benchmarks.

Run only after compilation and other tests have stopped. Child scenarios are
executed as CLIs, never imported. Every sample and log is retained for review.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
SCENARIOS = {'hosted': 'hosted_benchmark.py', 'rule': 'benchmark.py'}


def digest(path):
    value = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def binary_hashes(directory):
    extension = '.exe' if os.name == 'nt' else ''
    return {name: digest(directory / (name + extension))
            for name in ('astra-proxy', 'astra-rule')}


def trial_command(args, family, directory, output):
    common = [args.python, str(ROOT / 'scripts' / SCENARIOS[family]),
              '--bin-dir', str(directory), '--rounds', '1', '--output', str(output)]
    if family == 'hosted':
        return common + ['--routes', 'direct', 'group', '--seconds', str(args.seconds),
                         '--streams', '4', '--idle-connections', '100', '--connections', '1000',
                         '--concurrency', '10', '--settle-seconds', '0.5']
    return common + ['--rules', '1000', '10000', '--concurrency', '1', '32', '--connections', '1000']


def normalize(family, report):
    """Keep metric units explicit; medians of per-round percentiles are not pooled percentiles."""
    rows = {}
    for row in report['results']:
        metrics = {f'setup_{name}_ms': row['setup_ms'][name] for name in ('p50', 'p95', 'p99')}
        issues = []
        if family == 'hosted':
            key = f'hosted/{row["mode"]}/{row["route"]}'
            metrics['throughput_mib_per_second'] = row['throughput']['echo_payload_mib_per_second']
            resources = row['throughput']['resources']
            for name in ('cpu_seconds_during_throughput', 'cpu_percent_one_core_100',
                         'sampled_peak_aggregate_rss_bytes', 'peak_process_count'):
                metrics[name] = resources.get(name)
            for name in ('baseline_aggregate_rss_bytes', 'idle_aggregate_rss_bytes',
                         'aggregate_rss_delta_bytes', 'rss_bytes_per_connection'):
                metrics[name] = row['idle_memory'].get(name)
            issues = resources.get('collection_errors', []) + row['idle_memory'].get('collection_errors', [])
            if resources.get('collector', '').startswith('unavailable'):
                issues.append('resource collector unavailable')
        else:
            key = f'rule/{row["rules"]}/{row["cache"]}/concurrency-{row["concurrency"]}'
            metrics['throughput_connections_per_second'] = row['completed_per_second_median']
            metrics.update({f'echo_{name}_ms': row['echo_ms'][name] for name in ('p50', 'p95', 'p99')})
        if key in rows:
            raise ValueError(f'Duplicate scenario in single-round report: {key}')
        for name, value in metrics.items():
            if value is not None and (not isinstance(value, (int, float)) or not math.isfinite(value)):
                raise ValueError(f'Invalid metric {key}/{name}: {value!r}')
        rows[key] = {'metrics': metrics, 'resource_issues': sorted(set(issues))}
    expected = ({f'hosted/{mode}/{route}' for mode in ('ordinary', 'hosted') for route in ('direct', 'group')}
                if family == 'hosted' else
                {f'rule/{count}/{cache}/concurrency-{concurrency}' for count in (1000, 10000)
                 for cache in ('disabled', 'warm') for concurrency in (1, 32)})
    if set(rows) != expected:
        raise ValueError(f'Incomplete {family} scenario set: {sorted(rows)}')
    return rows


def aggregate(trials):
    grouped = {}
    for trial in trials:
        for key, row in trial['scenarios'].items():
            grouped.setdefault(key, {'before': [], 'after': []})[trial['side']].append(row)
    scenarios, flags = {}, []
    for key, sides in sorted(grouped.items()):
        metrics = {}
        names = sorted({name for rows in sides.values() for row in rows for name in row['metrics']})
        for name in names:
            values = {side: [row['metrics'].get(name) for row in rows] for side, rows in sides.items()}
            valid = {side: [value for value in data if value is not None] for side, data in values.items()}
            medians = {side: statistics.median(data) if data else None for side, data in valid.items()}
            before, after = medians['before'], medians['after']
            change = ((after / before - 1) * 100
                      if before is not None and before > 0 and after is not None else None)
            metrics[name] = {**medians, 'change_percent': change,
                             'valid_samples': {side: len(data) for side, data in valid.items()},
                             'samples': values}
            if change is None:
                continue
            # Compare original values to avoid rounding exactly -3% below -3.
            regressed = ((name.startswith('throughput_') and after < before * .97)
                         or (name.endswith(('_p95_ms', '_p99_ms')) and after > before * 1.05))
            if regressed:
                flags.append({'scenario': key, 'metric': name, 'change_percent': change})
        scenarios[key] = {
            'rounds': {side: len(rows) for side, rows in sides.items()},
            'metrics': metrics,
            'resource_issues': {side: sorted({issue for row in rows for issue in row['resource_issues']})
                                for side, rows in sides.items()},
        }
    return {'scenarios': scenarios, 'regressions': flags}


def save(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    temporary.replace(path)


def run(args):
    args.before = args.before.resolve()
    args.after = args.after.resolve()
    args.output_dir = args.output_dir.resolve()
    if not shutil.which(args.python):
        raise FileNotFoundError(f'Child Python not found: {args.python}')
    directories = {'before': args.before, 'after': args.after}
    identity = {side: binary_hashes(path) for side, path in directories.items()}
    if (args.output_dir / 'comparison.json').exists():
        raise FileExistsError(f'Preserve existing comparison; select another --output-dir: {args.output_dir}')
    args.output_dir.mkdir(parents=True, exist_ok=True)
    scripts = {family: digest(ROOT / 'scripts' / name) for family, name in SCENARIOS.items()}
    report = {
        'schema_version': 1, 'status': 'running',
        'started_at_utc': datetime.now(timezone.utc).isoformat(),
        'platform': platform.platform(), 'logical_cpus': os.cpu_count(),
        'binary_directories': {side: str(path) for side, path in directories.items()},
        'binary_sha256': identity, 'scenario_script_sha256': scripts,
        'requested_pairs': args.rounds, 'maximum_pairs': args.max_rounds,
        'thresholds': {'throughput_decrease_percent': 3, 'p95_p99_increase_percent': 5},
        'method': 'Alternating paired trials; flip before/after and scenario-family order each pair. Each child runs one round. Aggregate medians of round metrics, not pooled latency quantiles. Local Python loopback load only; no TUN, external encrypted peers or production-capacity claim. Resource metrics come from hosted trials only.',
        'fixture_correction': 'benchmark.py explicitly disables direct_local for both builds, matching the hosted fixture. This selects the default-action fallback; direct_local was evaluated inside Rule Core after matching and did not bypass IPC. Cache misses call Rule Core, and warm cache hits bypass that call. The rule echo fixture actively closes after exactly 1024 bytes, and the client waits for remote EOF outside the latency timestamps; completed connections/second includes this close wait. This prevents macOS client ephemeral-port exhaustion at 1000 samples without changing OS settings or dropping samples.',
        'trials': [],
    }
    destination = args.output_dir / 'comparison.json'
    save(destination, report)
    env = dict(os.environ, TMPDIR='/tmp', RUST_LOG='warn')
    for key in ('ASTRA_HOSTED_EXPERIMENTAL', 'ASTRA_HOSTED', 'ASTRA_CONTROL_SOCKET'):
        env.pop(key, None)
    target_pairs = args.rounds
    pair = 0
    try:
        while pair < target_pairs:
            pair += 1
            sides = ('before', 'after') if pair % 2 else ('after', 'before')
            families = ('hosted', 'rule') if pair % 2 else ('rule', 'hosted')
            for family in families:
                for side in sides:
                    stem = f'pair-{pair:02d}-{family}-{side}'
                    output = args.output_dir / (stem + '.json')
                    logfile = args.output_dir / (stem + '.log')
                    command = trial_command(args, family, directories[side], output)
                    print(f'{stem} ({pair}/{target_pairs} pairs)', flush=True)
                    started = time.monotonic()
                    with logfile.open('w', encoding='utf-8') as log:
                        result = subprocess.run(command, cwd=ROOT, env=env, stdout=log,
                                                stderr=subprocess.STDOUT, timeout=args.timeout)
                    if result.returncode:
                        raise RuntimeError(f'{stem} failed ({result.returncode}); see {logfile}')
                    child = json.loads(output.read_text(encoding='utf-8'))
                    hashes = {name.removesuffix('.exe'): value for name, value in child['binary_sha256'].items()}
                    if hashes != identity[side] or binary_hashes(directories[side]) != identity[side]:
                        raise RuntimeError(f'{side} binaries changed during comparison')
                    report['trials'].append({'pair': pair, 'family': family, 'side': side,
                                             'seconds': time.monotonic() - started, 'command': command,
                                             'report': output.name, 'log': logfile.name,
                                             'scenarios': normalize(family, child)})
                    save(destination, report)
            report['aggregate'] = aggregate(report['trials'])
            report['completed_pairs'] = pair
            if pair == args.rounds and report['aggregate']['regressions'] and args.max_rounds > args.rounds:
                report['initial_regressions'] = report['aggregate']['regressions']
                target_pairs = args.max_rounds
                print(f'Thresholds exceeded; extending to {target_pairs} alternating pairs', flush=True)
            save(destination, report)
            if pair < target_pairs:
                time.sleep(args.pause_seconds)
        if scripts != {family: digest(ROOT / 'scripts' / name) for family, name in SCENARIOS.items()}:
            raise RuntimeError('Benchmark scripts changed during comparison')
        report['status'] = 'complete'
        report['performance_gate_passed'] = not report['aggregate']['regressions']
    except BaseException as error:
        report['status'] = 'incomplete'
        report['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        report['finished_at_utc'] = datetime.now(timezone.utc).isoformat()
        save(destination, report)
    print(f'Comparison: {destination}; performance gate: {report["performance_gate_passed"]}', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, default=ROOT / 'target/optimization-baseline/bin')
    parser.add_argument('--after', type=Path, default=ROOT / 'target/optimization-after/bin')
    parser.add_argument('--output-dir', type=Path, default=ROOT / 'target/optimization-results/performance')
    parser.add_argument('--python', default='python3', help='child interpreter; choose one with psutil for resource measurements')
    parser.add_argument('--rounds', type=int, default=5)
    parser.add_argument('--max-rounds', type=int, default=10)
    parser.add_argument('--seconds', type=float, default=3)
    parser.add_argument('--timeout', type=float, default=300, help='maximum seconds for each child scenario')
    parser.add_argument('--pause-seconds', type=float, default=1)
    args = parser.parse_args()
    if args.rounds < 1 or args.max_rounds < args.rounds or args.seconds < 3 or args.timeout <= 0 or args.pause_seconds < 0:
        parser.error('require 1 <= rounds <= max-rounds, seconds >= 3, positive timeout and nonnegative pause')
    run(args)


if __name__ == '__main__':
    main()
