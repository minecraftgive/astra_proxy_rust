#!/usr/bin/env python3
"""Local end-to-end proxy groups: rule references, probing, switching, UDP and rollback."""
import argparse
import contextlib
import json
import os
import pathlib
import socket
import socketserver
import subprocess
import tempfile
import threading
import time
from platform_support import executable, process_flags
from protocol_matrix import ROOT, UdpEcho, UdpEchoServer, free_port, wait_port, udp_probe
from smoke_test import socks_connect


def headers(stream):
    data = bytearray()
    while not data.endswith(b'\r\n\r\n'):
        part = stream.recv(1)
        if not part:
            raise ConnectionError('closed before headers')
        data.extend(part)
        if len(data) > 8192:
            raise ValueError('headers too large')
    return bytes(data)


class Upstream(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        try:
            self.request.settimeout(5)
            assert headers(self.request).startswith(b'CONNECT example.test:80 ')
            self.request.sendall(b'HTTP/1.1 200 Connection established\r\n\r\n')
            request = headers(self.request)
            probe = request.startswith(b'GET /204 ')
            if probe:
                time.sleep(self.server.delay)
            status = 503 if probe and not self.server.healthy else 200
            body = self.server.marker.encode()
            self.request.sendall(f'HTTP/1.1 {status} Test\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n'.encode() + body)
        except (OSError, AssertionError):
            pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    with tempfile.TemporaryDirectory(prefix='astra groups 中文 ') as td, contextlib.ExitStack() as stack:
        directory = pathlib.Path(td)
        servers = []
        for marker, delay in [('a', .01), ('b', .25)]:
            server = stack.enter_context(Upstream(('127.0.0.1', 0), Handler))
            server.marker, server.delay, server.healthy = marker, delay, True
            threading.Thread(target=server.serve_forever, daemon=True).start()
            stack.callback(server.shutdown)
            servers.append(server)
        udp = stack.enter_context(UdpEchoServer(('127.0.0.1', 0), UdpEcho))
        threading.Thread(target=udp.serve_forever, daemon=True).start()
        stack.callback(udp.shutdown)
        route = directory/'route.sock'
        rule = directory/'rule.toml'
        proxy = directory/'proxy.toml'
        rule.write_text(f'socket={json.dumps(route.as_posix())}\ndefault_action="proxy:main"\ndirect_local=false\n', encoding='utf-8')
        port = free_port()

        def config(selected='speed', cycle=False, manual=False, automatic=False, failure=False, dynamic_filter=None, nested=False):
            text = f'rule_socket={json.dumps(route.as_posix())}\nsocks5_listen="127.0.0.1:{port}"\nshutdown_timeout_secs=1\ndecision_cache_ttl_secs=60\n'
            for server in servers:
                text += f'[[outbounds]]\nkind="http"\ntag="{server.marker}"\nserver="127.0.0.1:{server.server_address[1]}"\n'
            text += '[proxy_groups.speed]\nmode="auto"\nmembers=["a","b"]\nurl="http://example.test/204"\ninterval=1\ntimeout_ms=1000\ntolerance_ms=0\n'
            text += f'disconnect_on_auto={str(automatic).lower()}\ndisconnect_on_failure={str(failure).lower()}\n'
            members = ['speed', 'a', 'b', 'DIRECT', 'REJECT'] + (['main'] if cycle else [])
            if nested:
                members = ['speed']
            text += f'[proxy_groups.main]\nmode="select"\nmembers={json.dumps(members)}\nselected="{selected}"\n'
            text += f'disconnect_on_manual={str(manual).lower()}\n'
            if dynamic_filter is not None:
                text += '[proxy_groups.PROXY]\nmode="select"\ninclude_all=true\nfilter=' + json.dumps(dynamic_filter) + '\n'
            proxy.write_text(text, encoding='utf-8')

        @contextlib.contextmanager
        def core(name, path):
            binary = str(binaries/executable(name))
            control = directory/(name+'.ctl')
            log_path = directory/(name+'.log')
            with log_path.open('w', encoding='utf-8') as log:
                child = subprocess.Popen([binary, str(path)], stdout=log, stderr=subprocess.STDOUT,
                    env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(control)), **process_flags())
                try:
                    deadline = time.monotonic()+15
                    while not control.exists():
                        if child.poll() is not None or time.monotonic() > deadline:
                            raise RuntimeError(log_path.read_text(encoding='utf-8'))
                        time.sleep(.03)
                    yield binary, control, child, log_path
                finally:
                    if child.poll() is None:
                        subprocess.run([binary, '--stop', str(control)], check=True, timeout=10)
                        child.wait(timeout=15)
                    assert child.returncode == 0, log_path.read_text(encoding='utf-8')

        def request():
            client, status = socks_connect(port, 'example.test', 80)
            with client:
                if status != 0:
                    return None
                client.sendall(b'GET /traffic HTTP/1.1\r\nHost: example.test\r\n\r\n')
                data = bytearray()
                while True:
                    chunk = client.recv(4096)
                    if not chunk:
                        return bytes(data).split(b'\r\n\r\n', 1)[1].decode()
                    data.extend(chunk)

        def expect(value):
            deadline = time.monotonic()+12
            while True:
                if request() == value:
                    return
                if time.monotonic() > deadline:
                    raise AssertionError(f'group did not select {value!r}')
                time.sleep(.1)

        config(automatic=True, failure=True)
        with core('astra-rule', rule) as (rule_binary, rule_control, _, _), core('astra-proxy', proxy) as (binary, control, child, log_path):
            wait_port(port, child, log_path)
            expect('a')
            retained, status = socks_connect(port, 'example.test', 80)
            assert status == 0
            with retained:
                servers[0].delay, servers[1].delay = .25, .01
                expect('b')
                retained.settimeout(2)
                assert retained.recv(1) == b'', 'automatic switch must close previous stream'
            retained, status = socks_connect(port, 'example.test', 80)
            assert status == 0
            with retained:
                servers[1].healthy = False
                expect('a')
                retained.settimeout(2)
                assert retained.recv(1) == b'', 'failure switch must close previous stream'
            servers[0].healthy = False
            expect(None)
            servers[0].healthy = True
            expect('a')
            # Retain one established connection while changing the configured choice.
            retained, status = socks_connect(port, 'example.test', 80)
            assert status == 0
            with retained:
                config('b')
                subprocess.run([binary, '--reload', str(control)], check=True, timeout=10)
                assert request() == 'b', 'manual selection ignored'
                retained.sendall(b'GET /traffic HTTP/1.1\r\nHost: example.test\r\n\r\n')
                data = b''
                while True:
                    chunk = retained.recv(4096)
                    if not chunk:
                        break
                    data += chunk
                assert data.endswith(b'a'), 'existing stream changed its outbound'
            retained, status = socks_connect(port, 'example.test', 80)
            assert status == 0
            with retained:
                config('a', manual=True)
                subprocess.run([binary, '--reload', str(control)], check=True, timeout=10)
                retained.settimeout(2)
                assert retained.recv(1) == b'', 'manual switch policy did not close old stream'
            retained, status = socks_connect(port, 'example.test', 80)
            assert status == 0
            with retained:
                config('b', cycle=True, manual=True)
                result = subprocess.run([binary, '--reload', str(control)], capture_output=True, timeout=10)
                assert result.returncode != 0, 'cyclic group reload was accepted'
                assert request() == 'a', 'failed reload changed selected group'
                retained.sendall(b'GET /traffic HTTP/1.1\r\nHost: example.test\r\n\r\n')
                data = b''
                while True:
                    chunk = retained.recv(4096)
                    if not chunk:
                        break
                    data += chunk
                assert data.endswith(b'a'), 'failed reload disconnected an existing stream'
            config('b', nested=True)
            subprocess.run([binary, '--reload', str(control)], check=True, timeout=10)
            assert request() == 'b', 'parent must pin a nested node despite the child auto-selection'
            config('a', nested=True)
            subprocess.run([binary, '--reload', str(control)], check=True, timeout=10)
            assert request() == 'a', 'switching a nested node must change actual traffic'
            config('REJECT')
            subprocess.run([binary, '--reload', str(control)], check=True, timeout=10)
            denied, status = socks_connect(port, 'example.test', 80)
            denied.close()
            assert status == 2, 'group REJECT must behave as a blocked route'
            config('DIRECT')
            subprocess.run([binary, '--reload', str(control)], check=True, timeout=10)
            udp_probe(port, udp.server_address[1], sizes=[0, 32, 8192], hosts=['127.0.0.1'])
            rule.write_text(f'socket={json.dumps(route.as_posix())}\ndefault_action="proxy:PROXY"\ndirect_local=false\n', encoding='utf-8')
            subprocess.run([rule_binary, '--reload', str(rule_control)], check=True, timeout=10)
            config(dynamic_filter='^a$')
            subprocess.run([binary, '--reload', str(control)], check=True, timeout=10)
            expect('a')
            config(dynamic_filter='^b$')
            subprocess.run([binary, '--reload', str(control)], check=True, timeout=10)
            expect('b')
            config(dynamic_filter='^missing$')
            subprocess.run([binary, '--reload', str(control)], check=True, timeout=10)
            expect(None)
            config(dynamic_filter='^a$')
            subprocess.run([binary, '--reload', str(control)], check=True, timeout=10)
            expect('a')

    print('PASS proxy groups: cached rule reference, automatic latency/failure/recovery, manual reload, keep/close policies for automatic/failure/manual switches, cyclic rollback, grouped REJECT, grouped UDP DIRECT, dynamic default PROXY/filter reload/empty recovery')


if __name__ == '__main__':
    main()
