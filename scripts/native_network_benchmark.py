#!/usr/bin/env python3
"""Paired loopback proxy measurements using one shared native load generator.

Uses the established process/configuration fixtures, but neither the load nor
the echo service runs on Python's event loop. Retains failed and noisy samples.
"""
import argparse
import contextlib
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import threading
import time

from astra_tools.net import free_port
from astra_tools.platform import executable
from astra_tools.processes import wait_port, wait_socket
from benchmark import rules_text
from hosted_benchmark import Resources, digest, proxy_process, trial_config
from optimization_benchmark import aggregate, binary_hashes, save


def trial(directory, driver, scenario, connections, seconds):
    mode, route_kind, rules, cache, concurrency = scenario
    with tempfile.TemporaryDirectory(prefix='an-', dir='/private/tmp' if platform.system() == 'Darwin' else None) as temporary:
        root = Path(temporary)
        rule_root = root / 'rule'
        rule_root.mkdir()
        route = rule_root / 'route.sock'
        port = free_port()
        rule_text, proxy_text = trial_config(route, port, route_kind, 4096)
        if rules:
            rule_text = rules_text(rules, route)
            proxy_text += f'decision_cache_ttl_secs={cache}\n'
        rule_config = rule_root / 'rule.toml'
        config = root / 'proxy.toml'
        rule_config.write_text(rule_text)
        config.write_text(proxy_text)
        with contextlib.ExitStack() as stack:
            rule, rule_log = stack.enter_context(proxy_process(directory / executable('astra-rule'), rule_config, rule_root, False))
            wait_socket(route, rule, rule_log)
            proxy, proxy_log = stack.enter_context(proxy_process(directory / executable('astra-proxy'), config, root, mode == 'hosted'))
            wait_port(port, proxy, proxy_log)
            resources = Resources(proxy.pid, rule.pid)
            start_cpu = resources.sample()
            stop = threading.Event()
            def monitor():
                while not stop.wait(.1):
                    resources.sample()
            sampler = threading.Thread(target=monitor, daemon=True)
            sampler.start()
            started = time.monotonic()
            try:
                child = subprocess.run([str(driver), 'network', str(port), str(concurrency), str(connections), str(seconds if not rules else 0)], capture_output=True, text=True, timeout=180)
                if child.returncode:
                    raise RuntimeError(child.stderr + proxy_log.read_text(errors='replace')[-4000:])
                row = json.loads(child.stdout)
            finally:
                stop.set()
                sampler.join()
            elapsed = time.monotonic() - started
            usage = resources.result(start_cpu, resources.sample(), elapsed)
            row['resources'] = usage
            row['process_cpu_seconds'] = usage.pop('cpu_seconds_during_throughput')
            return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, required=True)
    parser.add_argument('--driver', type=Path, required=True)
    parser.add_argument('--output-dir', type=Path, required=True)
    parser.add_argument('--rounds', type=int, default=5)
    parser.add_argument('--max-rounds', type=int, default=10)
    parser.add_argument('--connections', type=int, default=5000)
    parser.add_argument('--seconds', type=int, default=3)
    args = parser.parse_args()
    if args.rounds < 5 or args.max_rounds < args.rounds or args.connections < 1000 or args.seconds < 3:
        parser.error('require >= 5 paired rounds, >= 1000 connections and >= 3 seconds')
    directories = {side: getattr(args, side).resolve() for side in ('before', 'after')}
    driver = args.driver.resolve()
    hashes = {side: binary_hashes(path) for side, path in directories.items()}
    driver_hash = digest(driver)
    destination = args.output_dir / 'comparison.json'
    if destination.exists():
        parser.error('preserve the existing report; choose a new output directory')
    args.output_dir.mkdir(parents=True, exist_ok=True)
    scenarios = [(mode, route, 0, 60, 10) for mode in ('ordinary', 'hosted') for route in ('direct', 'group')]
    scenarios += [('ordinary', 'direct', rules, cache, concurrency) for rules in (1000, 10000) for cache in (0, 60) for concurrency in (1, 32)]
    report = {'status': 'running', 'started_at_utc': datetime.now(timezone.utc).isoformat(),
              'platform': platform.platform(), 'logical_cpus': os.cpu_count(),
              'binary_sha256': hashes, 'driver_sha256': driver_hash,
              'connections': args.connections, 'stream_seconds': args.seconds,
              'method': 'One shared native client/echo binary, current-thread Tokio runtime. 512 warmup connections. Fixed concurrency, 1 KiB echo and remote EOF per connection. Setup/echo latency excludes EOF wait; connection throughput includes it. 4 persistent 64 KiB streams for ordinary/hosted throughput. Medians of alternating paired rounds; no dropped samples. CPU/RSS cover proxy tree plus rule, excluding load generator. Local unencrypted loopback only.',
              'thresholds': {'throughput_decrease_percent': 3, 'p95_p99_increase_percent': 5}, 'trials': []}
    save(destination, report)
    target = args.rounds
    pair = 0
    try:
        while pair < target:
            pair += 1
            order = scenarios if pair % 2 else list(reversed(scenarios))
            for scenario in order:
                mode, route, rules, cache, concurrency = scenario
                name = (f'rule/{rules}/{"warm" if cache else "disabled"}/concurrency-{concurrency}' if rules else f'hosted/{mode}/{route}')
                for side in (('before', 'after') if pair % 2 else ('after', 'before')):
                    print(f'pair {pair}/{target} {name} {side}', flush=True)
                    row = trial(directories[side], driver, scenario, args.connections, args.seconds)
                    stem = f'{pair:02d}-{name.replace("/", "-")}-{side}.json'
                    save(args.output_dir / stem, row)
                    metrics = {f'{phase}_{p}_ms': row[f'{phase}_ms'][p] for phase in ('setup', 'echo') for p in ('p50', 'p95', 'p99')}
                    metrics.update({key: row[key] for key in ('throughput_connections_per_second', 'throughput_mib_per_second', 'process_cpu_seconds') if key in row})
                    metrics['sampled_peak_aggregate_rss_bytes'] = row['resources']['sampled_peak_aggregate_rss_bytes']
                    report['trials'].append({'pair': pair, 'side': side, 'report': stem,
                                            'scenarios': {name: {'metrics': metrics, 'resource_issues': row['resources']['collection_errors']}}})
                    save(destination, report)
            report['aggregate'] = aggregate(report['trials'])
            report['completed_pairs'] = pair
            if pair == args.rounds and report['aggregate']['regressions']:
                report['initial_regressions'] = report['aggregate']['regressions']
                target = args.max_rounds
            save(destination, report)
        if hashes != {side: binary_hashes(path) for side, path in directories.items()} or driver_hash != digest(driver):
            raise RuntimeError('binaries changed during measurement')
        report.update(status='complete', performance_gate_passed=not report['aggregate']['regressions'])
    except BaseException as error:
        report.update(status='incomplete', error=str(error))
        raise
    finally:
        report['finished_at_utc'] = datetime.now(timezone.utc).isoformat()
        save(destination, report)
    print('performance gate:', report['performance_gate_passed'])


if __name__ == '__main__':
    main()
