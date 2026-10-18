#!/usr/bin/env python3
"""Local release baseline: SOCKS setup + 1 KiB echo, cold and warm route cache."""
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
import tempfile
import time
from protocol_matrix import ROOT, free_port, process, wait_port
from core_regression import wait_socket


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def percentile(values, percent):
    return sorted(values)[max(0, math.ceil(len(values) * percent / 100) - 1)]


async def echo(reader, writer):
    try:
        while data := await reader.read(65536):
            writer.write(data)
            await writer.drain()
    finally:
        writer.close()
        await writer.wait_closed()


async def query(proxy_port, target_port):
    start = time.perf_counter()
    reader, writer = await asyncio.open_connection('127.0.0.1', proxy_port)
    try:
        writer.write(b'\x05\x01\x00')
        await writer.drain()
        assert await reader.readexactly(2) == b'\x05\x00'
        writer.write(b'\x05\x01\x00\x01\x7f\x00\x00\x01' + target_port.to_bytes(2, 'big'))
        await writer.drain()
        assert (await reader.readexactly(10))[1] == 0
        connected = time.perf_counter()
        data = bytes(range(256)) * 4
        writer.write(data)
        await writer.drain()
        assert await reader.readexactly(len(data)) == data
        ended = time.perf_counter()
        return (connected - start) * 1000, (ended - start) * 1000
    finally:
        writer.close()
        await writer.wait_closed()


async def samples(port, target, count, concurrency):
    gate = asyncio.Semaphore(concurrency)
    async def one():
        async with gate:
            return await asyncio.wait_for(query(port, target), 10)
    start = time.perf_counter()
    result = await asyncio.gather(*(one() for _ in range(count)))
    return result, time.perf_counter() - start


def rules_text(count, sock):
    rows = [f'socket="{sock}"\ndefault_action="direct"\n']
    for index in range(count):
        kind, value = [
            ('domain_exact', f'host{index}.example.invalid'),
            ('domain_suffix', f'suffix{index}.example.invalid'),
            ('ip_cidr', f'198.18.{(index // 256) % 256}.{index % 256}/32'),
            ('port', str(1 + index % 1024)),
        ][index % 4]
        rows.append(f'[[rules]]\nkind="{kind}"\nvalue="{value}"\naction="direct"\n')
    return ''.join(rows)


async def run(args):
    binaries = args.bin_dir.resolve()
    report = {
        'platform': platform.platform(), 'machine': platform.machine(),
        'logical_cpus': os.cpu_count(), 'python': platform.python_version(),
        'binary_sha256': {name: digest(binaries / name) for name in ('astra-proxy', 'astra-rule')},
        'connections_per_round': args.connections, 'rounds': args.rounds,
        'scope': 'Local IPv4 direct SOCKS5 setup plus 1024-byte echo; includes Python client/server overhead. Not encrypted throughput or production capacity. Queue wait before admission to the client concurrency gate is excluded from latency.',
        'results': [],
    }
    server = await asyncio.start_server(echo, '127.0.0.1', 0, backlog=256)
    target = server.sockets[0].getsockname()[1]
    async with server:
        for count in args.rules:
            with tempfile.TemporaryDirectory(prefix='astra-bench-') as td, contextlib.ExitStack() as stack:
                directory = pathlib.Path(td)
                sock = directory / 'rules.sock'
                rule_path = directory / 'rules.toml'
                rule_path.write_text(rules_text(count, sock))
                rule, log = stack.enter_context(process([str(binaries/'astra-rule'), str(rule_path)], directory, 'rule'))
                wait_socket(sock, rule, log)
                for ttl in (0, 60):
                    port = free_port()
                    proxy_path = directory / 'proxy.toml'
                    proxy_path.write_text(f'rule_socket="{sock}"\nsocks5_listen="127.0.0.1:{port}"\ndecision_cache_ttl_secs={ttl}\nmax_connections=4096\nshutdown_timeout_secs=1\n')
                    with process([str(binaries/'astra-proxy'), str(proxy_path)], directory, f'proxy-{ttl}') as (proxy, proxy_log):
                        wait_port(port, proxy, proxy_log)
                        for concurrency in args.concurrency:
                            await samples(port, target, 32, min(32, concurrency))
                            latencies, rates = [], []
                            for _ in range(args.rounds):
                                result, elapsed = await samples(port, target, args.connections, concurrency)
                                latencies.extend(result)
                                rates.append(args.connections / elapsed)
                            setup = [pair[0] for pair in latencies]
                            full = [pair[1] for pair in latencies]
                            row = {'rules': count, 'cache': 'disabled' if ttl == 0 else 'warm',
                                   'concurrency': concurrency, 'samples': len(latencies),
                                   'setup_ms': {f'p{p}': round(percentile(setup, p), 3) for p in (50, 95, 99)},
                                   'echo_ms': {f'p{p}': round(percentile(full, p), 3) for p in (50, 95, 99)},
                                   'completed_per_second_median': round(statistics.median(rates), 1)}
                            report['results'].append(row)
                            print(json.dumps(row), flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT/'target/release')
    parser.add_argument('--rules', nargs='+', type=int, default=[1000, 10000])
    parser.add_argument('--concurrency', nargs='+', type=int, default=[1, 32])
    parser.add_argument('--connections', type=int, default=300)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--output', type=pathlib.Path, default=ROOT/'docs/performance-baseline.json')
    args = parser.parse_args()
    if min(args.rules + args.concurrency + [args.connections, args.rounds]) <= 0:
        parser.error('counts must be positive')
    asyncio.run(run(args))


if __name__ == '__main__':
    main()
