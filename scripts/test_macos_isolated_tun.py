#!/usr/bin/env python3
"""Prepare/run a manually authorized macOS TUN acceptance with owned test state.

Without --execute this only prints the review plan. It never invokes sudo. The
only optional explicit route is one unused benchmark host route; no default,
production profile, system proxy, DNS setting or physical interface is changed.
"""
import argparse
import ipaddress
import json
import os
import platform
import signal
import socket
import subprocess
import tempfile
import time
import traceback
from pathlib import Path

NETWORK = ipaddress.ip_network('198.19.254.0/30')
LOCAL, PEER = '198.19.254.1', '198.19.254.2'
HOST_ROUTE = PEER + '/32'


def run(command, *, env=None, timeout=10, check=True, input=None):
    return subprocess.run(command, env=env, timeout=timeout, check=check,
                          input=input, text=True, capture_output=True)


def interfaces():
    return set(run(['/sbin/ifconfig', '-l']).stdout.split())


def routes(family='inet'):
    rows = []
    for line in run(['/usr/sbin/netstat', '-rn', '-f', family]).stdout.splitlines():
        parts = line.split()
        if len(parts) >= 4 and parts[0] not in ('Destination', 'Routing', 'Internet:', 'Internet6:'):
            rows.append(tuple(parts[:4]))  # destination, gateway, flags, interface
    return sorted(rows)


def route_network(destination, flags):
    if destination == 'default':
        return ipaddress.ip_network('0.0.0.0/0')
    value, _, prefix = destination.partition('/')
    pieces = value.split('.')
    if not all(piece.isdigit() for piece in pieces) or len(pieces) > 4:
        return None
    length = int(prefix) if prefix else (32 if 'H' in flags else len(pieces) * 8)
    return ipaddress.ip_network('.'.join(pieces + ['0'] * (4 - len(pieces))) + '/' + str(length), strict=False)


def benchmark_routes():
    return [row for row in routes() if (network := route_network(row[0], row[2]))
            and network.prefixlen >= 15 and network.overlaps(NETWORK)]


def defaults():
    wanted = {'default', '0/1', '0.0/1', '0.0.0.0/1', '128/1', '128.0/1', '128.0.0.0/1', '::/1', '8000::/1'}
    return {family: [row for row in routes(family) if row[0] in wanted] for family in ('inet', 'inet6')}


def proxy_state():
    return run(['/usr/sbin/scutil', '--proxy']).stdout


def runtime(binary, control, env, action='snapshot', argument=None):
    command = [binary, '--runtime', str(control), action]
    if argument is not None:
        command.append(argument)
    return json.loads(run(command, env=env, timeout=10).stdout)


def stable_runtime(snapshot):
    return {key: snapshot[key] for key in ('pid', 'mode', 'tun', 'system_proxy', 'allow_lan')}


def diagnostic(binary, env, snapshot, egress, probe, *, expected_system_trace_rejection=False):
    request = {'egress': egress, 'probe': probe, 'count': 1, 'interval_ms': 0,
               'timeout_ms': 500, 'family': 'ipv4', 'path_context': {
                   'sampled_at_ms': str(time.time_ns() // 1_000_000),
                   'core_tun': snapshot['tun'], 'physical': None}}
    child = subprocess.Popen([binary, '--network-diagnostics-stream', '/tmp/unused-config'],
                             env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True)
    try:
        output, error = child.communicate(json.dumps(request), timeout=35)
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()
    events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
    if expected_system_trace_rejection:
        assert egress == {'kind': 'system'} and probe['kind'] == 'trace' and snapshot['tun'] is True
        assert child.returncode == 1, (child.returncode, error)
        summaries = [event for event in events if event.get('event') == 'trace_summary']
        assert len(summaries) == 1, (events, error)
        summary = summaries[0]
        assert summary['probes_sent'] == 0 and summary['capability']['supported'] is False
        assert not summary['reached'] and not summary['completed']
        assert not any(event.get('event') in ('resolution', 'trace', 'connection', 'attempt') for event in events)
    else:
        assert child.returncode == 0 and events and events[-1].get('event') == 'done', (child.returncode, error)
    return {'exit_code': child.returncode, 'events': events, 'stderr': error}


def nat_probe(binary, env, snapshot, interface, server):
    return diagnostic(binary, env, snapshot, {'kind': 'interface', 'name': interface}, {
        'kind': 'nat', 'request': {'agent_id': 'isolated-macos-tun-test', 'interface_id': interface,
                                 'family': 'ipv4', 'servers': [server], 'advanced': False}})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--interface', required=True, help='Explicit physical egress, e.g. en0')
    parser.add_argument('--stun-server', required=True, help='Explicit private IPv4 fixture IP:port')
    parser.add_argument('--protected-control', required=True, help='Existing m1 socket: read-only snapshots only')
    parser.add_argument('--protected-pid', type=int, required=True)
    parser.add_argument('--local-echo-control', action='store_true', help='Own only 198.19.254.2/32 and run one local ping')
    parser.add_argument('--execute', action='store_true', help='Run only after a separate manual root authorization')
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    host, port = args.stun_server.rsplit(':', 1)
    address = ipaddress.IPv4Address(host)
    if not address.is_private or address.is_loopback or address.is_unspecified or address.is_multicast or not 1 <= int(port) <= 65535:
        parser.error('Use a specified private-network fixture IPv4 address and port')
    explicit_routes = [HOST_ROUTE] if args.local_echo_control else []
    plan = {'scenario': 'macos_isolated_tun', 'binary': binary, 'physical_interface': args.interface,
            'stun_fixture': args.stun_server, 'tun_address': LOCAL + '/30', 'explicit_routes': explicit_routes,
            'fixture_preflight': 'Require two physical STUN replies before creating the temporary core or TUN',
            'protected_control_read_only': args.protected_control, 'protected_pid': args.protected_pid,
            'system_proxy_changes': False, 'default_route_changes': False,
            'temporary_state': '/private/tmp/astra-macos-tun-*/{config,runtime,core.sock,core.log}',
            'cleanup': 'Only own core: TUN off, Stop + wait; verify own interface/routes gone and protected state unchanged'}
    if not args.execute:
        print(json.dumps(plan, indent=2))
        return
    if platform.system() != 'Darwin' or os.geteuid() != 0:
        parser.error('--execute requires a separately authenticated macOS root session; this script never elevates')
    report = {'plan': plan, 'uid': os.geteuid(), 'passed': False, 'checks': {}}
    original_interfaces = interfaces()
    assert args.interface in original_interfaces and not args.interface.startswith(('utun', 'lo'))
    assert not benchmark_routes(), 'Benchmark test prefix already has a specific route; nothing started'
    initial_defaults, initial_proxy = defaults(), proxy_state()
    original_runtime = stable_runtime(runtime(binary, args.protected_control, None))
    assert original_runtime['pid'] == args.protected_pid, 'Protected core identity changed; nothing started'
    report['protected_before'] = original_runtime
    name = next('utun' + str(n) for n in range(200, 300) if 'utun' + str(n) not in original_interfaces)
    report['owned_interface'] = name
    child = None
    directory = tempfile.TemporaryDirectory(prefix='astra-macos-tun-', dir='/private/tmp')
    root = Path(directory.name)
    control, log_path = root / 'core.sock', root / 'core.log'
    env = {key: value for key, value in os.environ.items() if not key.startswith('ASTRA_')}
    env.update(ASTRA_CONTROL_SOCKET=str(control), ASTRA_RUNTIME_DIR=str(root / 'runtime'), ASTRA_LOG_LEVEL='info')
    log = log_path.open('w')
    def interrupted(signum, frame):
        raise RuntimeError('Interrupted; cleaning only the owned temporary core')
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, interrupted)
    try:
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            port = listener.getsockname()[1]
        config = root / 'proxy.toml'
        config.write_text(f'''standalone=true
standalone_outbound="ISOLATED"
socks5_listen="127.0.0.1:{port}"
shutdown_timeout_secs=1
[proxy_groups.ISOLATED]
mode="select"
members=["REJECT"]
selected="REJECT"
[tun]
name={json.dumps(name)}
address="{LOCAL}"
prefix=30
mtu=1500
interface={json.dumps(args.interface)}
auto_interface=false
auto_gateway=false
sniff=false
routes={json.dumps(explicit_routes)}
bypass=[]
''')
        run([binary, '--check', str(config)], env=env)
        baseline = nat_probe(binary, env, {'tun': False}, args.interface, args.stun_server)
        report['checks']['physical_nat_before_tun'] = baseline
        baseline_report = next(e['report'] for e in baseline['events'] if e.get('kind') == 'report')
        assert (not baseline_report['partial'] and len(baseline_report['observations']) == 2
                and all(item['response_received'] for item in baseline_report['observations'])
                and baseline_report['physical_path']['binding_verified']
                and baseline_report['physical_path']['route_verified']), \
            'STUN fixture preflight failed; no temporary core or TUN was started. Start a live fixture before retrying.'
        child = subprocess.Popen([binary, str(config)], env=env, stdout=log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 15
        while not control.exists():
            assert child.poll() is None and time.monotonic() < deadline, 'Temporary core did not start'
            time.sleep(0.05)
        before = runtime(binary, control, env)
        assert before['pid'] == child.pid and not before['tun'] and not before['system_proxy']
        active = runtime(binary, control, env, 'tun', 'on')
        assert active['pid'] == child.pid and active['tun'] and not active['system_proxy']
        assert name in interfaces()
        assert LOCAL in run(['/sbin/ifconfig', name]).stdout
        assert defaults() == initial_defaults and proxy_state() == initial_proxy
        assert stable_runtime(runtime(binary, args.protected_control, env)) == original_runtime
        report['runtime_active'] = stable_runtime(active)
        report['owned_routes_active'] = benchmark_routes()
        if args.local_echo_control:
            selected = run(['/sbin/route', '-n', 'get', '-inet', PEER]).stdout
            fields = dict(line.strip().split(':', 1) for line in selected.splitlines() if ':' in line)
            assert fields.get('interface', '').strip() == name, 'Owned host route did not select the temporary TUN'
            ping = run(['/sbin/ping', '-n', '-c', '1', '-W', '1000', PEER], timeout=4, check=False)
            report['local_echo_control'] = {'exit_code': ping.returncode, 'reply_observed': ping.returncode == 0,
                                            'stdout': ping.stdout, 'stderr': ping.stderr,
                                            'meaning': 'One local TUN-path control; never evidence of Internet reachability'}
        echo = diagnostic(binary, env, active, {'kind': 'system'}, {'kind': 'icmp', 'host': PEER})
        observation = next(e['observation'] for e in echo['events'] if e.get('event') == 'echo')
        assert observation['tun_local'] and not observation['proves_reachability'] and observation['rtt_ms'] is None
        assert not any(e.get('event') in ('resolution', 'connection') for e in echo['events'])
        report['checks']['system_icmp'] = echo
        trace = diagnostic(binary, env, active, {'kind': 'system'}, {'kind': 'trace', 'host': PEER, 'max_hops': 1, 'probes_per_hop': 1}, expected_system_trace_rejection=True)
        summary = next(e for e in trace['events'] if e.get('event') == 'trace_summary')
        assert summary['probes_sent'] == 0 and not summary['capability']['supported']
        report['checks']['system_trace'] = trace
        virtual = nat_probe(binary, env, active, name, args.stun_server)
        result = next(e['report'] for e in virtual['events'] if e.get('kind') == 'report')
        assert result['partial'] and not result['observations']
        report['checks']['virtual_nat_rejected'] = virtual
        physical = nat_probe(binary, env, active, args.interface, args.stun_server)
        result = next(e['report'] for e in physical['events'] if e.get('kind') == 'report')
        assert not result['partial'] and len(result['observations']) == 2
        assert all(item['response_received'] for item in result['observations'])
        assert result['physical_path']['binding_verified'] and result['physical_path']['route_verified']
        report['checks']['physical_nat'] = physical
        assert stable_runtime(runtime(binary, control, env)) == stable_runtime(active)
        assert stable_runtime(runtime(binary, args.protected_control, env)) == original_runtime
        report['passed'] = True
    except BaseException:
        report['error'] = traceback.format_exc()
    finally:
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(sig, signal.SIG_IGN)
        cleanup_errors = []
        if child is not None and child.poll() is None:
            try:
                off = runtime(binary, control, env, 'tun', 'off')
                assert off['pid'] == child.pid and not off['tun']
                run([binary, '--stop', str(control)], env=env)
                child.wait(timeout=10)
            except BaseException:
                cleanup_errors.append(traceback.format_exc())
            finally:
                if child.poll() is None:
                    child.terminate()
                    try: child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait(timeout=5)
        log.close()
        try:
            deadline = time.monotonic() + 5
            while name in interfaces() and time.monotonic() < deadline:
                time.sleep(0.1)
            assert name not in interfaces(), 'Owned TUN interface remains'
            assert not benchmark_routes(), 'Owned benchmark route remains'
            assert defaults() == initial_defaults, 'Default/half-default routes changed'
            assert proxy_state() == initial_proxy, 'System proxy state changed'
            report['protected_after'] = stable_runtime(runtime(binary, args.protected_control, env))
            assert report['protected_after'] == original_runtime, 'Protected core state changed'
        except BaseException:
            cleanup_errors.append(traceback.format_exc())
        report['cleanup_errors'] = cleanup_errors
        report['cleanup_verified'] = not cleanup_errors
        report['passed'] &= not cleanup_errors
        report['core_log'] = log_path.read_text()[-65536:]
        fd, output = tempfile.mkstemp(prefix='astra-macos-isolated-tun-', suffix='.json', dir='/private/tmp')
        with os.fdopen(fd, 'w') as stream:
            json.dump(report, stream, indent=2)
        if 'SUDO_UID' in os.environ:
            os.chown(output, int(os.environ['SUDO_UID']), int(os.environ['SUDO_GID']))
        directory.cleanup()
        print('Evidence:', output)
        print('PASS' if report['passed'] else 'FAILED; inspect evidence and cleanup_errors')
    raise SystemExit(0 if report['passed'] else 1)


if __name__ == '__main__':
    main()
