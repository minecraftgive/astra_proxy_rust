#!/usr/bin/env python3
"""Opt-in physical-interface invalidation test inside a disposable Colima VM.

Only the explicitly named VM interface is renamed. Its IPs, link state and routes
are never edited. The same guest Python process restores the name in finally.
The guest network service may briefly withdraw/reacquire DHCP during rename;
the test waits for exact address/route/rule recovery without editing them.
Host listeners bind loopback; no host/Windows interface is changed.
"""
import argparse
import json
import socket
import struct
import subprocess
import threading

GUEST = r'''
import difflib, json, signal, socket, subprocess, threading, time
binary = BINARY
interface = INTERFACE
renamed = 'astraifcheck0'
port = PORT
control_port = CONTROL_PORT

def ip(*args):
    return subprocess.check_output(['ip', *args], text=True, timeout=2)

def snapshot():
    # Lifetimes tick during a test; address identity and all route attributes do not.
    links = json.loads(ip('-j', 'address', 'show'))
    for link in links:
        for address in link.get('addr_info', []):
            address.pop('valid_life_time', None)
            address.pop('preferred_life_time', None)
    routes = json.loads(ip('-j', 'route', 'show', 'table', 'all'))
    rules = json.loads(ip('-j', 'rule', 'show'))
    for link in links:
        link['addr_info'].sort(key=lambda address: json.dumps(address, sort_keys=True))
    links.sort(key=lambda link: link['ifindex'])
    routes.sort(key=lambda route: json.dumps(route, sort_keys=True))
    rules.sort(key=lambda rule: json.dumps(rule, sort_keys=True))
    return json.dumps([links, routes, rules], sort_keys=True, indent=2)

def interrupted(signum, frame):
    raise RuntimeError('Interrupted; restoring selected interface')

for sig in (signal.SIGHUP, signal.SIGTERM, signal.SIGINT):
    signal.signal(sig, interrupted)
assert subprocess.run(['ip', 'link', 'show', renamed], capture_output=True).returncode != 0, 'Temporary name already exists'
before = snapshot()
original = json.loads(ip('-j', 'link', 'show', interface))[0]
assert 'UP' in original['flags'] and original['link_type'] == 'ether'

request = {
    'egress': {'kind': 'interface', 'name': interface},
    'probe': {'kind': 'nat', 'request': {
        'agent_id': 'isolated-linux-interface-test', 'interface_id': interface,
        'family': 'ipv4', 'servers': [f'192.168.5.2:{port}'], 'advanced': False,
    }},
    'count': 1, 'interval_ms': 0, 'timeout_ms': 1000, 'family': 'ipv4',
    'path_context': {'sampled_at_ms': '0', 'core_tun': False, 'physical': None},
}

def probe():
    child = subprocess.Popen([binary, '--network-diagnostics-stream', '/tmp/unused-config'],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        output, error = child.communicate(json.dumps(request), timeout=12)
    finally:
        if child.poll() is None:
            child.kill()
        child.wait()
    events = [json.loads(line) for line in output.splitlines() if line.startswith('{')]
    report = next(e['report'] for e in events if e.get('kind') == 'report')
    return child.returncode, report, error

control = None
worker = None
changed = threading.Event()
failures = []
stop_rename = threading.Event()
rename_lock = threading.Lock()
try:
    _, baseline, _ = probe()
    assert not baseline['partial'] and baseline['physical_path']['route_verified'], baseline
    assert len(baseline['observations']) == 2
    print('BASELINE_PHYSICAL_NAT_TWO_RESPONSES', flush=True)
    control = socket.create_connection(('192.168.5.2', control_port), timeout=5)
    control.settimeout(8)
    reader = control.makefile('rb')
    control.sendall(b'ARM\n')
    assert reader.readline() == b'ARMED\n'

    def rename_on_first_request():
        try:
            assert reader.readline() == b'FIRST_REQUEST\n'
            with rename_lock:
                if stop_rename.is_set():
                    return
                subprocess.run(['ip', 'link', 'set', 'dev', interface, 'name', renamed], check=True, capture_output=True, timeout=2)
                changed.set()
                current = json.loads(ip('-j', 'link', 'show', renamed))[0]
                assert current['ifindex'] == original['ifindex'] and current['address'] == original['address']
                assert 'UP' in current['flags'], 'Renaming must not take the link down'
                control.sendall(b'RENAMED\n')
        except BaseException as error:
            failures.append(repr(error))
            try: control.sendall(b'RENAME_FAILED\n')
            except OSError: pass

    worker = threading.Thread(target=rename_on_first_request, daemon=True)
    worker.start()
    code, report, error = probe()
    worker.join(timeout=2)
    assert not worker.is_alive() and not failures and changed.is_set(), failures
    assert report['partial'], report
    assert report['mapping'] == 'unknown' and report['filtering'] == 'unknown'
    assert report['traditional_type'] is None
    assert report['physical_path']['binding_verified'] is False
    assert report['physical_path']['route_verified'] is False
    assert not report['observations'], 'The delayed response must not be accepted after interface invalidation'
    assert any('Stopped without changing interface' in note for note in report['notes']), report
    print('INTERFACE_RENAMED_WHILE_FIRST_RESPONSE_PENDING', flush=True)
    print('NAT_STOPPED_PARTIAL_WITH_INVALIDATED_PATH', json.dumps(report, sort_keys=True), flush=True)
    print('PROBE_EXIT', code, error.strip(), flush=True)
finally:
    # Coordinate cancellation with the rename thread: no late rename may occur
    # after the restoration check has completed.
    stop_rename.set()
    if control is not None:
        try: control.shutdown(socket.SHUT_RDWR)
        except OSError: pass
    with rename_lock:
        # Only the same device (index + MAC) is eligible for restoration.
        if subprocess.run(['ip', 'link', 'show', renamed], capture_output=True).returncode == 0:
            current = json.loads(ip('-j', 'link', 'show', renamed))[0]
            assert current['ifindex'] == original['ifindex'] and current['address'] == original['address']
            subprocess.run(['ip', 'link', 'set', 'dev', renamed, 'name', interface], check=True, timeout=2)
    if control is not None:
        control.close()
    # networkd can withdraw/reacquire DHCP when an interface name changes.
    # Do not edit addresses/routes to hide that: wait boundedly for exact recovery.
    restored_at = time.monotonic()
    after = snapshot()
    while after != before and time.monotonic() - restored_at < 10:
        time.sleep(0.1)
        after = snapshot()
    print('NETWORK_SERVICE_RECOVERY_SECONDS', round(time.monotonic() - restored_at, 3), flush=True)
    if after != before:
        print('STATE_DIFFERENCE', '\n'.join(difflib.unified_diff(before.splitlines(), after.splitlines(), fromfile='before', tofile='after')), flush=True)
    assert after == before, 'Interface addresses, route or rule state changed'
    print('RESTORED_INTERFACE_AND_UNCHANGED_ADDRESSES_ROUTES_RULES', flush=True)
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile', required=True)
    parser.add_argument('--binary', required=True, help='Absolute Linux astra-proxy path in the VM')
    parser.add_argument('--interface', default='eth0')
    args = parser.parse_args()
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    udp.bind(('127.0.0.1', 0))
    tcp.bind(('127.0.0.1', 0))
    tcp.listen(1)
    tcp.settimeout(20)
    armed, first, ready = (threading.Event() for _ in range(3))
    packets, failures = [], []

    def coordinate():
        try:
            with tcp.accept()[0] as conn:
                conn.settimeout(10)
                reader = conn.makefile('rb')
                assert reader.readline() == b'ARM\n'
                armed.set()
                conn.sendall(b'ARMED\n')
                assert first.wait(8), 'No STUN request after arming'
                conn.sendall(b'FIRST_REQUEST\n')
                assert reader.readline() == b'RENAMED\n', 'Guest could not rename its live interface'
        except BaseException as error:
            failures.append(repr(error))
        finally:
            ready.set()

    def serve():
        while True:
            try:
                data, peer = udp.recvfrom(4096)
                if len(data) < 20 or data[:2] != b'\x00\x01':
                    continue
                active = armed.is_set()
                packets.append({'phase': 'invalidated' if active else 'baseline', 'peer': peer, 'transaction': data[8:20].hex()})
                if active and not first.is_set():
                    first.set()
                    assert ready.wait(10), 'Guest rename did not complete'
                masked = bytes(a ^ b for a, b in zip(socket.inet_aton(peer[0]), struct.pack('!I', 0x2112A442)))
                body = struct.pack('!HHBBH', 0x20, 8, 0, 1, peer[1] ^ 0x2112) + masked
                udp.sendto(struct.pack('!HHI', 0x101, len(body), 0x2112A442) + data[8:20] + body, peer)
            except OSError:
                return
            except BaseException as error:
                failures.append(repr(error))
                return

    threading.Thread(target=coordinate, daemon=True).start()
    threading.Thread(target=serve, daemon=True).start()
    code = GUEST.replace('binary = BINARY', 'binary = ' + repr(args.binary))
    code = code.replace('interface = INTERFACE', 'interface = ' + repr(args.interface))
    code = code.replace('port = PORT', 'port = ' + str(udp.getsockname()[1]))
    code = code.replace('control_port = CONTROL_PORT', 'control_port = ' + str(tcp.getsockname()[1]))
    try:
        result = subprocess.run(['colima', 'ssh', '--profile', args.profile, '--', 'sudo', '-n', 'python3', '-c', code],
                                capture_output=True, text=True, timeout=45)
        print(result.stdout, end='')
        print(result.stderr, end='')
        invalidated = [packet for packet in packets if packet['phase'] == 'invalidated']
        print('HOST_STUN_PACKETS', json.dumps(packets))
        assert not failures, failures
        assert result.returncode == 0, 'Guest validation failed'
        assert len(invalidated) == 1, 'A second request/retry or fallback escaped the invalidated physical guard'
        print('PASS_ONE_REQUEST_NO_SECOND_ROUND_OR_EGRESS_FALLBACK')
    finally:
        udp.close()
        tcp.close()


if __name__ == '__main__':
    main()
