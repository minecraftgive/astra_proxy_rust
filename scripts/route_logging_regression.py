#!/usr/bin/env python3
"""Check Route thresholds and real TCP/UDP forwarding without system changes."""
import argparse
import contextlib
import http.server
import json
import os
import pathlib
import socket
import subprocess
import tempfile
import threading
import time

from platform_support import executable, process_flags
from protocol_matrix import Echo, EchoServer, UdpEcho, UdpEchoServer, exact, free_port, udp_probe, wait_port
from smoke_test import socks_connect


class HttpEcho(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Length', '2')
        self.end_headers()
        self.wfile.write(b'ok')

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    with tempfile.TemporaryDirectory(prefix='astra-route-') as temporary, contextlib.ExitStack() as stack:
        directory = pathlib.Path(temporary)
        echo = stack.enter_context(EchoServer(('127.0.0.1', 0), Echo))
        udp = stack.enter_context(UdpEchoServer(('127.0.0.1', 0), UdpEcho))
        http_server = stack.enter_context(http.server.ThreadingHTTPServer(('127.0.0.1', 0), HttpEcho))
        for server in [echo, udp, http_server]:
            threading.Thread(target=server.serve_forever, daemon=True).start()
            stack.callback(server.shutdown)

        @contextlib.contextmanager
        def core(name, config, level):
            endpoint = directory / f'{name}-{level}.ctl'
            log = directory / f'{name}-{level}.log'
            binary = str(binaries / executable(name))
            with log.open('w', encoding='utf-8') as output:
                child = subprocess.Popen([binary, str(config)], stdout=output, stderr=subprocess.STDOUT,
                                         env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(endpoint), ASTRA_LOG_LEVEL=level), **process_flags())
                try:
                    yield child, log
                finally:
                    if child.poll() is None:
                        result = subprocess.run([binary, '--stop', str(endpoint)], capture_output=True, timeout=10)
                        if result.returncode:
                            child.kill()
                    child.wait(timeout=15)
                assert child.returncode == 0, log.read_text(encoding='utf-8')

        route = directory / 'rules.sock'
        rules = directory / 'rules.toml'
        rules.write_text('socket=' + json.dumps(route.as_posix()) + '\ndefault_action="direct"\n'
                         '[[rules]]\nkind="domain_exact"\nvalue="blocked.test"\naction="block"\n', encoding='utf-8')
        with core('astra-rule', rules, 'info') as (rule, rule_log):
            deadline = time.monotonic() + 10
            while not route.exists():
                assert rule.poll() is None and time.monotonic() < deadline, rule_log.read_text(encoding='utf-8')
                time.sleep(.02)
            for level in ['info', 'route', 'debug']:
                socks, connect = free_port(), free_port()
                config = directory / f'{level}.toml'
                config.write_text('rule_socket=' + json.dumps(route.as_posix()) + f'\nsocks5_listen="127.0.0.1:{socks}"\nhttp_listen="127.0.0.1:{connect}"\nshutdown_timeout_secs=1\n', encoding='utf-8')
                secret = b'route-payload-must-not-appear'
                with core('astra-proxy', config, level) as (proxy, log):
                    wait_port(socks, proxy, log)
                    for _ in range(2):
                        client, status = socks_connect(socks, '127.0.0.1', echo.server_address[1])
                        with client:
                            assert status == 0
                            client.sendall(secret)
                            assert exact(client, len(secret)) == secret
                            client.shutdown(socket.SHUT_WR)
                            assert client.recv(1) == b''
                    blocked, status = socks_connect(socks, 'blocked.test', 443)
                    blocked.close()
                    assert status == 2
                    failed, status = socks_connect(socks, '127.0.0.1', free_port())
                    failed.close()
                    assert status == 5
                    with socket.create_connection(('127.0.0.1', connect), timeout=5) as client:
                        client.sendall(f'CONNECT 127.0.0.1:{echo.server_address[1]} HTTP/1.1\r\n\r\n'.encode())
                        headers = b''
                        while b'\r\n\r\n' not in headers:
                            headers += client.recv(1024)
                        assert headers.startswith(b'HTTP/1.1 200')
                        client.sendall(secret)
                        assert exact(client, len(secret)) == secret
                    with socket.create_connection(('127.0.0.1', connect), timeout=5) as client:
                        client.sendall(f'GET http://127.0.0.1:{http_server.server_address[1]}/private-token HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n'.encode())
                        response = b''
                        while chunk := client.recv(4096):
                            response += chunk
                        assert b' 200 ' in response.split(b'\r\n', 1)[0] and response.endswith(b'ok'), response
                    udp_probe(socks, udp.server_address[1], sizes=[0, 17], hosts=['127.0.0.1'])
                text = log.read_text(encoding='utf-8')
                assert secret.decode() not in text and '/private-token' not in text
                if level == 'info':
                    assert ' ROUTE ' not in text
                else:
                    route_lines = [line for line in text.splitlines() if ' ROUTE ' in line]
                    detail = '\n'.join(route_lines)
                    for expected in ['phase="route"', 'phase="forward"', 'phase="closed"', 'phase="failed"', 'source=Some(127.0.0.1:', 'network=Tcp', 'network=Udp', 'inbound=Socks5', 'inbound=HttpConnect', 'action=Direct', 'action=Block', 'elapsed_ms=', 'packets_up=2', 'packets_down=2', f'up={len(secret)} down={len(secret)}']:
                        assert expected in detail, (expected, text)
                    assert 'DEBUG astra_route' not in text
                    if level == 'route':
                        assert ' DEBUG ' not in text
    print('PASS Route thresholds, cached routing, block/failure, SOCKS/HTTP/UDP, byte counts, and metadata-only logs')


if __name__ == '__main__':
    main()
