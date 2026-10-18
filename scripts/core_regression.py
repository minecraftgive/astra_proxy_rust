#!/usr/bin/env python3
"""Black-box regressions for admission, fail-closed routing and rule restart."""
import argparse
import contextlib
import pathlib
import socket
import tempfile
import threading
import time
from protocol_matrix import ROOT, Echo, EchoServer, exact, free_port, process, wait_port


def socks(port, target_port):
    sock = socket.create_connection(('127.0.0.1', port), timeout=3)
    sock.sendall(b'\x05\x01\x00')
    assert exact(sock, 2) == b'\x05\x00'
    sock.sendall(b'\x05\x01\x00\x01\x7f\x00\x00\x01' + target_port.to_bytes(2, 'big'))
    return sock, exact(sock, 10)[1]


def wait_socket(path, child, log):
    for _ in range(100):
        if path.exists():
            return
        if child.poll() is not None:
            raise RuntimeError(log.read_text())
        time.sleep(.02)
    raise TimeoutError('rule socket not ready')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT / 'target/debug')
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    with tempfile.TemporaryDirectory(prefix='astra-regression-') as td, EchoServer(('127.0.0.1', 0), Echo) as echo:
        directory = pathlib.Path(td)
        threading.Thread(target=echo.serve_forever, daemon=True).start()
        destination = echo.server_address[1]
        rule_path = directory / 'rule.toml'
        socket_path = directory / 'rule.sock'
        rule_path.write_text(f'socket="{socket_path}"\ndefault_action="direct"\n')
        sp, hp = free_port(), free_port()
        proxy_path = directory / 'proxy.toml'
        proxy_path.write_text(f'rule_socket="{socket_path}"\nsocks5_listen="127.0.0.1:{sp}"\nhttp_listen="127.0.0.1:{hp}"\nmax_connections=1\ndecision_cache_ttl_secs=0\nshutdown_timeout_secs=1\n')
        with process([str(binaries / 'astra-rule'), str(rule_path)], directory, 'rule') as (rule, rule_log):
            wait_socket(socket_path, rule, rule_log)
            with process([str(binaries / 'astra-proxy'), str(proxy_path)], directory, 'proxy') as (proxy, proxy_log):
                wait_port(hp, proxy, proxy_log)
                # An idle SOCKS listener must not reserve the only session permit.
                with socket.create_connection(('127.0.0.1', hp), timeout=3) as client:
                    client.sendall(f'CONNECT 127.0.0.1:{destination} HTTP/1.1\r\n\r\n'.encode())
                    assert client.recv(1024).startswith(b'HTTP/1.1 200')
                    client.sendall(b'http')
                    assert exact(client, 4) == b'http'
                client, status = socks(sp, destination)
                with client:
                    assert status == 0
                    client.sendall(b'socks')
                    assert exact(client, 5) == b'socks'
                print('PASS max_connections=1: both idle-listener paths', flush=True)
            # Restart with capacity for the established stream and new probes.
            proxy_path.write_text(proxy_path.read_text().replace('max_connections=1', 'max_connections=8'))
            with process([str(binaries / 'astra-proxy'), str(proxy_path)], directory, 'proxy-restart') as (proxy, log):
                wait_port(sp, proxy, log)
                existing, status = socks(sp, destination)
                with existing:
                    assert status == 0
                    rule.terminate()
                    rule.wait(timeout=3)
                    assert not socket_path.exists(), 'SIGTERM left stale rule socket'
                    denied, status = socks(sp, destination)
                    denied.close()
                    assert status == 1, status
                    existing.sendall(b'alive')
                    assert exact(existing, 5) == b'alive'
                    rule_path.write_text(f'socket="{socket_path}"\ndefault_action="block"\n')
                    with process([str(binaries / 'astra-rule'), str(rule_path)], directory, 'rule-restarted') as (new_rule, new_log):
                        wait_socket(socket_path, new_rule, new_log)
                        denied, status = socks(sp, destination)
                        denied.close()
                        assert status == 2, status
                        existing.sendall(b'still-alive')
                        assert exact(existing, 11) == b'still-alive'
                print('PASS rule outage/restart: fail closed, established TCP survives, SIGTERM cleans socket', flush=True)
        echo.shutdown()

if __name__ == '__main__':
    main()
