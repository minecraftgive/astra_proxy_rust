#!/usr/bin/env python3
"""Compare ordinary and resident-worker SOCKS5 using loopback echo only.

Never enables TUN, OS proxy settings, transparent interception or external nodes.
Optional psutil measures the complete proxy process tree (including workers).
Measures direct and proxy-group routes, including retained idle tunnel memory.
"""
from astra_tools.workspace import resolve_binary_dir
import argparse
import asyncio
import contextlib
import hashlib
import json
import math
import os
import pathlib
import platform
import statistics
import subprocess
import tempfile
import threading
import time
from datetime import datetime, timezone

from astra_tools.platform import executable, interrupt, process_flags
from astra_tools.workspace import ROOT
from astra_tools.net import free_port
from astra_tools.processes import wait_port
from astra_tools.processes import wait_socket

try:
    import psutil
except ImportError:
    psutil = None


def percentile(values, percent):
    return sorted(values)[max(0, math.ceil(len(values) * percent / 100) - 1)]


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


@contextlib.contextmanager
def proxy_process(binary, config, directory, hosted):
    environment = dict(os.environ)
    for key in ('ASTRA_HOSTED_EXPERIMENTAL', 'ASTRA_HOSTED', 'ASTRA_CONTROL_SOCKET'):
        environment.pop(key, None)
    environment['ASTRA_CONTROL_SOCKET'] = str(directory / 'control.sock')
    environment['RUST_LOG'] = 'warn'
    if hosted:
        environment['ASTRA_HOSTED_EXPERIMENTAL'] = '1'
    options = process_flags()
    if os.name == 'nt':
        startup = subprocess.STARTUPINFO()
        startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
        startup.wShowWindow = subprocess.SW_HIDE
        options['startupinfo'] = startup
    log_path = directory / 'proxy.log'
    with log_path.open('w', encoding='utf-8') as log:
        child = subprocess.Popen([str(binary), str(config)], cwd=ROOT, env=environment,
                                 stdin=subprocess.DEVNULL, stdout=log, stderr=log, **options)
        try:
            yield child, log_path
        finally:
            descendants = []
            if psutil is not None:
                with contextlib.suppress(psutil.Error):
                    descendants = psutil.Process(child.pid).children(recursive=True)
            if child.poll() is None:
                try:
                    interrupt(child)
                    child.wait(timeout=10)
                except (OSError, subprocess.TimeoutExpired):
                    child.kill()
                    child.wait(timeout=10)
            # Only descendants captured from our owned process are considered.
            # This also cleans workers if a failed benchmark forced the host down.
            if descendants:
                _, remaining = psutil.wait_procs(descendants, timeout=3)
                for descendant in remaining:
                    with contextlib.suppress(psutil.Error):
                        descendant.kill()
                psutil.wait_procs(remaining, timeout=3)


class Resources:
    def __init__(self, *pids):
        self.roots = [psutil.Process(pid) for pid in pids] if psutil is not None else []
        self.cpu = {}
        self.peak_rss = 0
        self.current_rss = None
        self.peak_processes = 0
        self.errors = set()
        self.process_names = set()
        self.samples = 0
        self.per_process = {}
        self.lock = threading.Lock()

    def sample(self):
        with self.lock:
            return self._sample()

    def rss(self):
        with self.lock:
            self._sample()
            return self.current_rss if not self.errors else None

    def _sample(self):
        self.current_rss = None
        if not self.roots:
            return None
        try:
            processes = {process.pid: process for root in self.roots
                         for process in [root] + root.children(recursive=True)}.values()
        except psutil.Error as error:
            self.errors.add(type(error).__name__)
            return None
        total_rss = 0
        alive = 0
        for process in processes:
            try:
                identity = (process.pid, process.create_time())
                cpu = process.cpu_times()
                self.cpu[identity] = max(self.cpu.get(identity, 0), cpu.user + cpu.system)
                rss = process.memory_info().rss
                total_rss += rss
                record = self.per_process.setdefault(identity, {"pid": process.pid, "name": process.name(), "sampled_peak_rss_bytes": 0, "sampled_cpu_seconds": 0})
                record["sampled_peak_rss_bytes"] = max(record["sampled_peak_rss_bytes"], rss)
                record["sampled_cpu_seconds"] = max(record["sampled_cpu_seconds"], cpu.user + cpu.system)
                self.process_names.add(process.name())
                alive += 1
            except psutil.Error as error:
                self.errors.add(type(error).__name__)
        self.current_rss = total_rss
        self.samples += 1
        self.peak_rss = max(self.peak_rss, total_rss)
        self.peak_processes = max(self.peak_processes, alive)
        return sum(self.cpu.values())

    async def monitor(self):
        while True:
            # Windows process-tree enumeration must not stall the load generator.
            await asyncio.to_thread(self.sample)
            await asyncio.sleep(.1)

    def result(self, start_cpu, end_cpu, elapsed):
        with self.lock:
            return self._result(start_cpu, end_cpu, elapsed)

    def _result(self, start_cpu, end_cpu, elapsed):
        available = start_cpu is not None and end_cpu is not None and not self.errors
        seconds = max(0, end_cpu - start_cpu) if available else None
        return {
            'collector': 'psutil proxy tree plus rule process' if self.roots else 'unavailable (install psutil)',
            'cpu_seconds_during_throughput': round(seconds, 6) if seconds is not None else None,
            'cpu_percent_one_core_100': round(seconds / elapsed * 100, 2) if seconds is not None else None,
            'sampled_peak_aggregate_rss_bytes': self.peak_rss if self.samples else None,
            'peak_process_count': self.peak_processes if self.samples else None,
            'process_names': sorted(self.process_names),
            'per_process': list(self.per_process.values()),
            'sample_interval_ms': 100,
            'samples': self.samples,
            'collection_errors': sorted(self.errors),
        }


async def echo_server(reader, writer):
    try:
        while data := await reader.read(65536):
            writer.write(data)
            await writer.drain()
    except (ConnectionError, asyncio.CancelledError):
        pass
    finally:
        writer.close()
        with contextlib.suppress(ConnectionError):
            await writer.wait_closed()


async def connect(proxy, destination):
    start = time.perf_counter()
    reader, writer = await asyncio.open_connection('127.0.0.1', proxy)
    try:
        writer.write(b'\x05\x01\x00')
        await writer.drain()
        if await reader.readexactly(2) != b'\x05\x00':
            raise RuntimeError('SOCKS greeting rejected')
        writer.write(b'\x05\x01\x00\x01\x7f\x00\x00\x01' + destination.to_bytes(2, 'big'))
        await writer.drain()
        reply = await reader.readexactly(10)
        if reply[1] != 0:
            raise RuntimeError(f'SOCKS CONNECT rejected: {reply[1]}')
        return reader, writer, (time.perf_counter() - start) * 1000
    except BaseException:
        writer.close()
        with contextlib.suppress(ConnectionError):
            await writer.wait_closed()
        raise


async def setup_samples(proxy, destination, count, concurrency):
    gate = asyncio.Semaphore(concurrency)

    async def one():
        async with gate:
            reader, writer, elapsed = await asyncio.wait_for(connect(proxy, destination), 10)
            try:
                writer.write(b'local benchmark')
                await writer.drain()
                if await asyncio.wait_for(reader.readexactly(15), 10) != b'local benchmark':
                    raise RuntimeError('echo payload mismatch')
                return elapsed
            finally:
                writer.close()
                await writer.wait_closed()
    return await asyncio.gather(*(one() for _ in range(count)))


async def throughput(proxy, destination, duration, streams, resources):
    channels = []
    try:
        for _ in range(streams):
            channels.append(await asyncio.wait_for(connect(proxy, destination), 10))
        payload = bytes(range(256)) * 256
        start_cpu = await asyncio.to_thread(resources.sample)
        started = time.perf_counter()
        deadline = started + duration

        async def transfer(reader, writer, _):
            transferred = 0
            while time.perf_counter() < deadline:
                writer.write(payload)
                await writer.drain()
                if await reader.readexactly(len(payload)) != payload:
                    raise RuntimeError('throughput payload mismatch')
                transferred += len(payload)
            return transferred
        transferred = sum(await asyncio.wait_for(
            asyncio.gather(*(transfer(*channel) for channel in channels)), duration + 15))
        elapsed = time.perf_counter() - started
        end_cpu = await asyncio.to_thread(resources.sample)
        return {
            'streams': streams,
            'payload_chunk_bytes': len(payload),
            'echoed_payload_bytes': transferred,
            'elapsed_seconds': round(elapsed, 6),
            'echo_payload_mib_per_second': round(transferred / elapsed / 1024 ** 2, 3),
            'resources': resources.result(start_cpu, end_cpu, elapsed),
        }
    finally:
        for _, writer, _ in channels:
            writer.close()
        for _, writer, _ in channels:
            with contextlib.suppress(ConnectionError):
                await writer.wait_closed()


async def idle_memory(proxy, destination, count, settle_seconds, resources):
    """Keep CONNECT tunnels alive while sampling, then close every owned socket."""
    async def settled_rss():
        await asyncio.sleep(settle_seconds)
        values = []
        for _ in range(5):
            value = await asyncio.to_thread(resources.rss)
            if value is None:
                return None
            values.append(value)
            await asyncio.sleep(.1)
        return int(statistics.median(values))

    baseline = await settled_rss()
    channels = []
    try:
        for _ in range(count):
            channel = await asyncio.wait_for(connect(proxy, destination), 10)
            channels.append(channel)
            reader, writer, _ = channel
            # Confirm the complete forwarding path has started before idling.
            writer.write(b'idle')
            await writer.drain()
            if await asyncio.wait_for(reader.readexactly(4), 10) != b'idle':
                raise RuntimeError('idle tunnel echo payload mismatch')
        retained = await settled_rss()
        if any(reader.at_eof() for reader, _, _ in channels):
            raise RuntimeError('idle tunnel closed before RSS sample')
        delta = retained - baseline if baseline is not None and retained is not None else None
        return {
            'connections': count,
            'settle_seconds': settle_seconds,
            'rss_samples_per_phase': 5,
            'baseline_aggregate_rss_bytes': baseline,
            'idle_aggregate_rss_bytes': retained,
            'aggregate_rss_delta_bytes': delta,
            'rss_bytes_per_connection': round(delta / count, 2) if delta is not None else None,
            'collection_errors': sorted(resources.errors),
        }
    finally:
        for _, writer, _ in channels:
            writer.close()
        for _, writer, _ in channels:
            with contextlib.suppress(ConnectionError):
                await writer.wait_closed()


def trial_config(route, port, route_kind, max_connections):
    action = 'proxy:bench' if route_kind == 'group' else 'direct'
    rule = (f'socket = {json.dumps(str(route))}\ndefault_action = "{action}"\n'
            'direct_local = false\n')
    proxy = (f'rule_socket = {json.dumps(str(route))}\nsocks5_listen = "127.0.0.1:{port}"\n'
             f'max_connections = {max_connections}\nshutdown_timeout_secs = 1\n')
    if route_kind == 'group':
        proxy += ('[proxy_groups.bench]\nmode = "select"\nmembers = ["DIRECT"]\n'
                  'selected = "DIRECT"\ndisconnect_on_manual = true\n')
    return rule, proxy


async def run(args):
    binary = args.bin_dir.resolve() / executable('astra-proxy')
    rule_binary = args.bin_dir.resolve() / executable('astra-rule')
    for required in (binary, rule_binary):
        if not required.is_file():
            raise FileNotFoundError(f'Build the current client binaries first: {required}')
    report = {
        'schema_version': 2,
        'timestamp_utc': datetime.now(timezone.utc).isoformat(),
        'platform': platform.platform(), 'machine': platform.machine(),
        'logical_cpus': os.cpu_count(), 'python': platform.python_version(),
        'binary': str(binary.relative_to(ROOT)) if binary.is_relative_to(ROOT) else str(binary),
        'binary_sha256': {path.name: digest(path) for path in (binary, rule_binary)},
        'build_profile_hint': args.bin_dir.name,
        'connections_per_round': args.connections, 'concurrency': args.concurrency,
        'warmup_connections': args.warmup, 'rounds': args.rounds,
        'settle_seconds_after_warmup': args.settle_seconds,
        'throughput_seconds_requested': args.seconds,
        'routes': args.routes, 'idle_connections': args.idle_connections,
        'scope': 'Same proxy/rule binaries, IPv4 SOCKS5 over loopback. Each trial has one Rule Core with an empty rules table and either direct action or a select proxy group pointing at DIRECT. Idle RSS is the median of five samples before and during N retained, echo-verified CONNECT tunnels; per-connection memory is (idle RSS - baseline RSS) / N, without clamping allocator noise. CPU/RSS include the rule process and proxy plus all worker descendants. Python asyncio client and echo server are excluded from CPU/RSS but limit measured throughput. No TUN, system proxy, DNS listener or external traffic. Setup latency excludes the client concurrency gate. Throughput counts acknowledged echo payload once, not upload plus download. RSS is sampled aggregate working set, not an OS lifetime peak.',
        'results': [],
    }
    server = await asyncio.start_server(echo_server, '127.0.0.1', 0, backlog=256)
    destination = server.sockets[0].getsockname()[1]
    async with server:
        for round_index in range(args.rounds):
            order = (False, True) if round_index % 2 == 0 else (True, False)
            for hosted in order:
                mode = 'hosted' if hosted else 'ordinary'
                for route_kind in args.routes:
                    with tempfile.TemporaryDirectory(prefix='astra-hosted-bench-') as temporary:
                        directory = pathlib.Path(temporary)
                        port = free_port()
                        rule_directory = directory / 'rule'
                        rule_directory.mkdir()
                        route = rule_directory / 'route.sock'
                        rule_config = rule_directory / 'rule.toml'
                        config = directory / 'proxy.toml'
                        rule_text, proxy_text = trial_config(route, port, route_kind, max(4096, args.idle_connections + args.concurrency + 16))
                        rule_config.write_text(rule_text, encoding='utf-8')
                        config.write_text(proxy_text, encoding='utf-8')
                        with contextlib.ExitStack() as stack:
                            rule, rule_log = stack.enter_context(proxy_process(rule_binary, rule_config, rule_directory, False))
                            await asyncio.to_thread(wait_socket, route, rule, rule_log)
                            child, log = stack.enter_context(proxy_process(binary, config, directory, hosted))
                            await asyncio.to_thread(wait_port, port, child, log)
                            await setup_samples(port, destination, args.warmup, args.concurrency)
                            await asyncio.sleep(args.settle_seconds)
                            resources = Resources(child.pid, rule.pid)
                            sampler = asyncio.create_task(resources.monitor())
                            try:
                                idle = await idle_memory(port, destination, args.idle_connections, args.settle_seconds, resources)
                                setup = await setup_samples(port, destination, args.connections, args.concurrency)
                                sustained = await throughput(port, destination, args.seconds, args.streams, resources)
                            except Exception as error:
                                raise RuntimeError(f'{mode}/{route_kind} failed: {error}\n{log.read_text(encoding="utf-8", errors="replace")[-4000:]}') from error
                            finally:
                                sampler.cancel()
                                with contextlib.suppress(asyncio.CancelledError):
                                    await sampler
                            row = {'mode': mode, 'route': route_kind, 'idle_memory': idle, 'round': round_index + 1, 'samples': len(setup),
                                   'setup_ms': {f'p{p}': round(percentile(setup, p), 3) for p in (50, 95, 99)},
                                   'throughput': sustained}
                            report['results'].append(row)
                            print(json.dumps(row), flush=True)
    report['summary'] = {}
    for mode in ('ordinary', 'hosted'):
        report['summary'][mode] = {}
        for route_kind in args.routes:
            rows = [row for row in report['results'] if row['mode'] == mode and row['route'] == route_kind]
            memory = [row['idle_memory']['rss_bytes_per_connection'] for row in rows
                      if row['idle_memory']['rss_bytes_per_connection'] is not None]
            report['summary'][mode][route_kind] = {
                'median_round_setup_p50_ms': round(statistics.median(row['setup_ms']['p50'] for row in rows), 3),
                'median_round_setup_p95_ms': round(statistics.median(row['setup_ms']['p95'] for row in rows), 3),
                'median_echo_payload_mib_per_second': round(statistics.median(row['throughput']['echo_payload_mib_per_second'] for row in rows), 3),
                'median_idle_rss_bytes_per_connection': round(statistics.median(memory), 2) if memory else None,
            }
    if report['binary_sha256'] != {path.name: digest(path) for path in (binary, rule_binary)}:
        raise RuntimeError('Binaries changed during the benchmark; rebuild first and rerun the comparison')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    print(f'Report: {args.output.resolve()}', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=None)
    parser.add_argument('--output', type=pathlib.Path, default=ROOT / 'docs/hosted-performance.json')
    parser.add_argument('--routes', nargs='+', choices=('direct', 'group'), default=['direct', 'group'])
    parser.add_argument('--idle-connections', type=int, default=100)
    parser.add_argument('--connections', type=int, default=100)
    parser.add_argument('--concurrency', type=int, default=1)
    parser.add_argument('--warmup', type=int, default=10)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--seconds', type=float, default=3)
    parser.add_argument('--streams', type=int, default=2)
    parser.add_argument('--settle-seconds', type=float, default=2)
    args = parser.parse_args(); args.bin_dir = resolve_binary_dir(args.bin_dir, profile='debug')
    if min(args.idle_connections, args.connections, args.concurrency, args.warmup, args.rounds, args.seconds, args.streams) <= 0:
        parser.error('all workload sizes and durations must be positive')
    if args.settle_seconds < 0:
        parser.error('--settle-seconds must be nonnegative')
    asyncio.run(run(args))


if __name__ == '__main__':
    main()
