#!/usr/bin/env python3
"""Paired native synthetic DNS/rule/log workloads with allocation counts and RSS.

Build scripts/memory-benchmark against each source tree first; the baseline uses
--features legacy to reproduce the original GUI row preparation. The harness
holds its workload for 250 ms after reporting; retained RSS is the median of the
final ten 10 ms samples. No GPU, TUN, external network, or production-capacity
claims. Requires psutil.
"""
import argparse
from collections import deque
import hashlib
import json
from pathlib import Path
import platform
import statistics
import subprocess
import time
import psutil


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def trial(binary, scenario):
    child = subprocess.Popen([str(binary), scenario], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    process = psutil.Process(child.pid)
    peak_rss = 0
    retained = deque(maxlen=10)
    cpu = 0
    deadline = time.monotonic() + 120
    try:
        while child.poll() is None:
            if time.monotonic() > deadline:
                raise TimeoutError(scenario)
            try:
                rss = process.memory_info().rss
                retained.append(rss)
                peak_rss = max(peak_rss, rss)
                times = process.cpu_times()
                cpu = max(cpu, times.user + times.system)
            except psutil.NoSuchProcess:
                pass
            time.sleep(.01)
        stdout, stderr = child.communicate(timeout=5)
        if child.returncode:
            raise RuntimeError(stderr)
        result = json.loads(stdout)
        result.update(sampled_peak_rss_bytes=peak_rss, retained_rss_bytes=statistics.median(retained), sampled_cpu_seconds=cpu, process_count=1)
        return result
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--before', type=Path, required=True)
    parser.add_argument('--after', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--rounds', type=int, default=5)
    args = parser.parse_args()
    if args.rounds < 5:
        parser.error('at least five paired rounds required')
    binaries = {side: getattr(args, side).resolve() for side in ('before', 'after')}
    hashes = {side: digest(path) for side, path in binaries.items()}
    report = {'platform': platform.platform(), 'binary_sha256': hashes, 'rounds': args.rounds,
              'scope': 'Native synthetic in-process workload; counted System allocator on both builds. RSS and CPU include the entire harness process; throughput/latency cover the named hot loop, excluding setup. Log legacy path mirrors old GUI row preparation; no GPU rendering. No production capacity claim.',
              'trials': [], 'status': 'running'}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    def save():
        args.output.write_text(json.dumps(report, indent=2) + '\n')
    try:
        for pair in range(args.rounds):
            scenarios = ('dns', 'fake', 'rules', 'logs')
            if pair % 2:
                scenarios = tuple(reversed(scenarios))
            for scenario in scenarios:
                for side in (('before', 'after') if pair % 2 == 0 else ('after', 'before')):
                    print(f'pair {pair+1} {scenario} {side}', flush=True)
                    report['trials'].append({'pair': pair+1, 'side': side, 'scenario': scenario, **trial(binaries[side], scenario)})
                    save()
        aggregate = {}
        regressions = []
        for scenario in sorted({row['scenario'] for row in report['trials']}):
            metrics = {}
            for name in ('operations_per_second','p95_us','p99_us','live_allocated_bytes','peak_allocated_bytes','sampled_peak_rss_bytes','retained_rss_bytes','sampled_cpu_seconds'):
                values = {side: statistics.median(row[name] for row in report['trials'] if row['scenario']==scenario and row['side']==side) for side in binaries}
                values['change_percent'] = (values['after']/values['before']-1)*100 if values['before'] else None
                metrics[name] = values
                if ((name=='operations_per_second' and values['after']<values['before']*.97)
                    or (name in ('p95_us','p99_us') and values['after']>values['before']*1.05)):
                    regressions.append({'scenario':scenario,'metric':name,**values})
            aggregate[scenario] = metrics
        report.update(aggregate=aggregate, regressions=regressions, status='complete')
        if hashes != {side: digest(path) for side, path in binaries.items()}:
            raise RuntimeError('binaries changed during comparison')
    except BaseException as error:
        report.update(status='incomplete', error=str(error))
        raise
    finally:
        save()


if __name__ == '__main__':
    main()
