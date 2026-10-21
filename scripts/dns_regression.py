#!/usr/bin/env python3
"""Exercise the built DNS service using private loopback ports, without OS changes."""
import argparse
import contextlib
import os
import pathlib
import socket
import socketserver
import struct
import subprocess
import tempfile
import threading
import secrets
import time
import tomllib
from platform_support import executable, interrupt, process_flags
from protocol_matrix import ROOT, exact, wait_port


def dns_port():
    # Windows UDP exclusions differ from TCP ephemeral reservations.
    for _ in range(128):
        with socket.socket(socket.AF_INET,socket.SOCK_STREAM) as tcp, socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as udp:
            try:
                # TCP ephemeral allocations can repeatedly land in a whole
                # UDP-excluded block on Windows after adapter/Hyper-V changes.
                port=20000+secrets.randbelow(25000)
                tcp.bind(('127.0.0.1',port))
                udp.bind(('127.0.0.1',port))
                return port
            except OSError:
                pass
    raise RuntimeError('no available local UDP/TCP test port')


def question(name, ident=1):
    labels = b''.join(bytes([len(label)]) + label.encode('ascii') for label in name.split('.')) + b'\0'
    return struct.pack('!6H', ident, 0x100, 1, 0, 0, 0) + labels + struct.pack('!2H', 1, 1)


class Upstream(socketserver.BaseRequestHandler):
    def handle(self):
        raw, sock = self.request
        self.server.queries += 1
        reply = struct.pack('!6H', int.from_bytes(raw[:2], 'big'), 0x8180, 1, 1, 0, 0) + raw[12:]
        reply += b'\xc0\x0c' + struct.pack('!2HIH', 1, 1, 1, 4) + socket.inet_aton('203.0.113.7')
        sock.sendto(reply, self.client_address)


def lookup(port, name, ident, tcp=False):
    raw = question(name, ident)
    if tcp:
        with socket.create_connection(('127.0.0.1', port), timeout=3) as client:
            client.sendall(len(raw).to_bytes(2, 'big') + raw)
            reply = exact(client, int.from_bytes(exact(client, 2), 'big'))
    else:
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
            client.settimeout(3)
            client.connect(('127.0.0.1', port))
            client.send(raw)
            reply = client.recv(4096)
    assert reply[:2] == raw[:2], 'transaction ID was not restored'
    assert reply[3] & 15 == 0, f'DNS error: {reply.hex()}'
    assert int.from_bytes(reply[6:8], 'big') == 1, 'expected one address answer'
    return socket.inet_ntoa(reply[-4:])


@contextlib.contextmanager
def running(core, config, control, directory):
    env = dict(os.environ, ASTRA_CONTROL_SOCKET=str(control))
    log_path = directory / (config.stem + '.log')
    with log_path.open('w', encoding='utf-8') as log:
        child = subprocess.Popen([str(core), str(config)], cwd=ROOT, env=env,
                                 stdout=log, stderr=subprocess.STDOUT, **process_flags())
        try:
            yield child, log_path, env
        finally:
            if child.poll() is None:
                interrupt(child)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT/'target/release')
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    core = binaries/executable('astra-proxy')
    with tempfile.TemporaryDirectory(prefix='astra-dns-') as temp, socketserver.UDPServer(('127.0.0.1', 0), Upstream) as upstream:
        directory = pathlib.Path(temp)
        upstream.queries = 0
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        try:
            port = dns_port()
            config = directory/'real.toml'
            config.write_text(f'[dns]\nlisten=["127.0.0.1:{port}"]\nnameserver=["udp://127.0.0.1:{upstream.server_address[1]}"]\n', encoding='utf-8')
            subprocess.run([str(core), '--check', str(config)], check=True, capture_output=True)
            with running(core, config, directory/'real-control', directory) as (child, log, _env):
                wait_port(port, child, log)
                for ident in range(100):
                    assert lookup(port, 'cache.example', ident, tcp=bool(ident % 2)) == '203.0.113.7'
                assert upstream.queries == 1, f'cache missed: {upstream.queries} upstream queries'
                time.sleep(1.1)
                assert lookup(port, 'cache.example', 101) == '203.0.113.7'
                assert upstream.queries == 2, 'expired cache entry was retained'
            port = dns_port()
            config = directory/'fake.toml'
            control = directory/'fake-control'
            source = f'[dns]\nlisten=["127.0.0.1:{port}"]\nfake_ip=true\nmapping_capacity=4\n'
            config.write_text(source, encoding='utf-8')
            with running(core, config, control, directory) as (child, log, env):
                wait_port(port, child, log)
                address = lookup(port, 'fake.example', 200)
                assert address.startswith('198.18.'), address
                for ident in range(201, 401):
                    assert lookup(port, 'fake.example', ident, tcp=bool(ident % 2)) == address
                config.write_text(source+'cache_capacity=16\n', encoding='utf-8')
                subprocess.run([str(core), '--reload', str(control)], env=env, check=True, capture_output=True)
                assert lookup(port, 'fake.example', 401, tcp=True) == address
                config.write_text(source.replace('fake_ip=true', 'fake_ip=false'), encoding='utf-8')
                result = subprocess.run([str(core), '--reload', str(control)], env=env, capture_output=True)
                assert result.returncode != 0, 'unsafe fake range shutdown was allowed during reload'
                assert lookup(port, 'fake.example', 402) == address, 'failed reload changed active DNS state'
            yaml = directory/'input.yaml'
            yaml.write_text('proxies:\n  - {name: node, type: socks5, server: 127.0.0.1, port: 9}\ndns:\n  enable: true\n  enhanced-mode: fake-ip\n  nameserver: [tls://1.1.1.1]\n', encoding='utf-8')
            output = directory/'imported'
            subprocess.run([str(binaries/executable('astra-convert')), str(yaml), '-o', str(output)], check=True, capture_output=True)
            imported = output/'proxy.toml'
            assert tomllib.loads(imported.read_text(encoding='utf-8'))['dns']['fake_ip'] is True
            subprocess.run([str(core), '--check', str(imported)], check=True, capture_output=True)
        finally:
            upstream.shutdown()
            thread.join(timeout=2)
    print('PASS DNS UDP/TCP, transaction IDs, TTL cache, Fake IP stability, atomic reload and Mihomo import')


if __name__ == '__main__':
    main()
