#!/usr/bin/env python3
"""Process-level smoke test for both separately running binaries.
Build first: cargo build --release. Works on Linux/macOS (UDS).
"""
from astra_tools.net import exact as recv_exact, socks_connect, Echo, EchoServer as ThreadedServer
from astra_tools.processes import wait_port as wait_for_tcp
import pathlib
import signal
import socket
import socketserver
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]





from astra_tools.net import free_port








def main():
    from astra_tools.workspace import resolve_binary_dir
    BIN = resolve_binary_dir()
    for name in ('astra-rule', 'astra-proxy'):
        if not (BIN / name).exists():
            raise SystemExit('missing release executable; run cargo build --release first')
    socks, http = free_port(), free_port()
    with ThreadedServer(('127.0.0.1', 0), Echo) as echo, tempfile.TemporaryDirectory() as td:
        echo_port = echo.server_address[1]
        echo_thread = __import__('threading').Thread(target=echo.serve_forever, daemon=True)
        echo_thread.start()
        sock_path = pathlib.Path(td) / 'rule.sock'
        cfg_rule = pathlib.Path(td) / 'rule.toml'
        cfg_proxy = pathlib.Path(td) / 'proxy.toml'
        cfg_rule.write_text(f'''socket = "{sock_path}"
default_action = "direct"
[[rules]]
kind = "domain_exact"
value = "blocked.test"
action = "block"
''')
        cfg_proxy.write_text(f'''rule_socket = "{sock_path}"
socks5_listen = "127.0.0.1:{socks}"
http_listen = "127.0.0.1:{http}"
''')
        processes = []
        try:
            processes.append(subprocess.Popen([str(BIN / 'astra-rule'), str(cfg_rule)], cwd=ROOT))
            for _ in range(100):
                if sock_path.exists():
                    break
                time.sleep(0.05)
            processes.append(subprocess.Popen([str(BIN / 'astra-proxy'), str(cfg_proxy)], cwd=ROOT))
            wait_for_tcp(socks, processes[-1])
            wait_for_tcp(http, processes[-1])

            # SOCKS5 -> Direct -> local echo
            conn, reply = socks_connect(socks, '127.0.0.1', echo_port)
            assert reply == 0, f'expected direct success, got {reply}'
            with conn:
                conn.sendall(b'astra-hello')
                assert recv_exact(conn, 11) == b'astra-hello'
            conn, reply = socks_connect(socks, 'blocked.test', 443)
            assert reply == 2, f'expected blocked, got {reply}'
            conn.close()

            # HTTP CONNECT -> Direct -> local echo
            with socket.create_connection(('127.0.0.1', http), timeout=5) as conn:
                conn.sendall(f'CONNECT 127.0.0.1:{echo_port} HTTP/1.1\r\nHost: localhost\r\n\r\n'.encode())
                assert conn.recv(256).startswith(b'HTTP/1.1 200')
                conn.sendall(b'http-hello')
                assert recv_exact(conn, 10) == b'http-hello'
            with socket.create_connection(('127.0.0.1', http), timeout=5) as conn:
                conn.sendall(b'CONNECT blocked.test:443 HTTP/1.1\r\nHost: blocked.test\r\n\r\n')
                assert conn.recv(256).startswith(b'HTTP/1.1 403')
            print('PASS: SOCKS5 direct/block, HTTP CONNECT direct/block, separate IPC processes')
        finally:
            for proc in reversed(processes):
                if proc.poll() is None:
                    proc.send_signal(signal.SIGINT)
                    try:
                        proc.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()
            echo.shutdown()

if __name__ == '__main__':
    main()
