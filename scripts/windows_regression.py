#!/usr/bin/env python3
"""Native local regression: IPC, TCP/UDP, reload, standalone and graceful exit."""
import argparse
import contextlib
import json
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from platform_support import executable, process_flags, interrupt
from protocol_matrix import ROOT, Echo, EchoServer, UdpEcho, UdpEchoServer, exact, free_port, wait_port, udp_probe
from smoke_test import socks_connect


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT/'target/debug')
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    with tempfile.TemporaryDirectory(prefix='astra windows 中文 ') as td, contextlib.ExitStack() as stack:
        directory = pathlib.Path(td)
        # Exercise Windows path escaping, spaces, Unicode, and longer state paths.
        directory /= ('long state directory ' * 3).strip()
        directory.mkdir()
        echo = stack.enter_context(EchoServer(('127.0.0.1', 0), Echo))
        udp = stack.enter_context(UdpEchoServer(('127.0.0.1', 0), UdpEcho))
        for listener in (echo, udp):
            threading.Thread(target=listener.serve_forever, daemon=True).start()
            stack.callback(listener.shutdown)

        @contextlib.contextmanager
        def core(name, config):
            control = directory/(name+'.ctl')
            binary = str(binaries/executable(name))
            log_path = directory/(name+'.log')
            with log_path.open('w', encoding='utf-8') as log:
                child = subprocess.Popen([binary, str(config)], stdout=log, stderr=subprocess.STDOUT,
                                         env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(control)), **process_flags())
                try:
                    deadline = time.monotonic()+15
                    while not control.exists():
                        if child.poll() is not None or time.monotonic() > deadline:
                            raise RuntimeError(log_path.read_text(encoding='utf-8'))
                        time.sleep(.03)
                    yield child, log_path, control
                finally:
                    if child.poll() is None:
                        subprocess.run([binary, '--stop', str(control)], check=True, timeout=10)
                        child.wait(timeout=15)
                    assert child.returncode == 0, log_path.read_text(encoding='utf-8')
                    assert not control.exists(), 'management endpoint was not cleaned up'

        route = directory/'route.sock'
        rules = directory/'rule.toml'
        proxy = directory/'proxy.toml'
        rules.write_text('socket='+json.dumps(route.as_posix())+'\ndefault_action="direct"\n[[rules]]\nkind="domain_exact"\nvalue="blocked.test"\naction="block"\n', encoding='utf-8')
        sp, hp = free_port(), free_port()
        proxy.write_text(f'rule_socket={json.dumps(route.as_posix())}\nsocks5_listen="127.0.0.1:{sp}"\nhttp_listen="127.0.0.1:{hp}"\ndecision_cache_ttl_secs=0\nshutdown_timeout_secs=1\n', encoding='utf-8')
        with core('astra-rule', rules) as (rule, _, rule_control), core('astra-proxy', proxy) as (app, log, proxy_control):
            wait_port(sp, app, log)
            client, status = socks_connect(sp, '127.0.0.1', echo.server_address[1])
            with client:
                assert status == 0
                payload = bytes(range(256))*256
                client.sendall(payload)
                assert exact(client, len(payload)) == payload
                client.shutdown(socket.SHUT_WR)
                assert client.recv(1) == b''
            denied, status = socks_connect(sp, 'blocked.test', 443)
            denied.close()
            assert status == 2
            with socket.create_connection(('127.0.0.1', hp), timeout=5) as client:
                client.sendall(f'CONNECT 127.0.0.1:{echo.server_address[1]} HTTP/1.1\r\n\r\n'.encode())
                assert client.recv(1024).startswith(b'HTTP/1.1 200')
                client.sendall(b'http-echo')
                assert exact(client, 9) == b'http-echo'
            udp_probe(sp, udp.server_address[1], sizes=[0, 1, 4096, 8192], hosts=['127.0.0.1', 'localhost'])
            # A standalone proxy must start with no astra-rule executable beside it.
            only_proxy = directory/'only proxy'
            only_proxy.mkdir()
            shutil.copy2(binaries/executable('astra-proxy'), only_proxy/executable('astra-proxy'))
            standalone = directory/'standalone.toml'
            port = free_port()
            standalone.write_text(f'standalone=true\nsocks5_listen="127.0.0.1:{port}"\nshutdown_timeout_secs=1\n[[outbounds]]\nkind="socks5"\ntag="upstream"\nserver="127.0.0.1:{sp}"\n', encoding='utf-8')
            launcher = [sys.executable, str(ROOT/'scripts/astra.py'), '--bin-dir', str(only_proxy), '--proxy', str(standalone), '--rules', str(directory/'missing.toml')]
            subprocess.run(launcher+['--check'], check=True, timeout=15)
            with (directory/'launcher.log').open('w', encoding='utf-8') as output:
                child = subprocess.Popen(launcher, stdout=output, stderr=subprocess.STDOUT, **process_flags())
                try:
                    wait_port(port, child, directory/'launcher.log')
                    client, status = socks_connect(port, '127.0.0.1', echo.server_address[1])
                    with client:
                        assert status == 0
                        client.sendall(b'standalone')
                        assert exact(client, 10) == b'standalone'
                    udp_probe(port, udp.server_address[1], sizes=[1, 4096], hosts=['127.0.0.1'])
                finally:
                    if child.poll() is None:
                        interrupt(child)
                    assert child.wait(timeout=20) == 0, (directory/'launcher.log').read_text(encoding='utf-8')
                # A forced launcher exit would leave its child bound here.
                with socket.socket() as probe:
                    probe.bind(('127.0.0.1', port))
            def reload_rules(settings):
                rules.write_text('socket='+json.dumps(route.as_posix())+'\ndefault_action="block"\n'+settings, encoding='utf-8')
                subprocess.run([str(binaries/executable('astra-rule')), '--reload', str(rule_control)], check=True, timeout=10)

            def expect_local_status(expected, message):
                client, status = socks_connect(sp, '127.0.0.1', echo.server_address[1])
                with client:
                    assert status == expected, f'{message}: expected {expected}, got {status}'
                    if status == 0:
                        client.sendall(b'local-default')
                        assert exact(client, 13) == b'local-default'

            # Local DIRECT precedes the default action, while explicit rules
            # precede local DIRECT. Cache is disabled above, so each request
            # observes the acknowledged reload without waiting for a push.
            reload_rules('')
            expect_local_status(0, 'local DIRECT default was not preserved after reload')
            reload_rules('[[rules]]\nkind="ip_cidr"\nvalue="127.0.0.0/8"\naction="block"\n')
            expect_local_status(2, 'reloaded explicit local block was not applied')
            reload_rules('direct_local=false\n')
            expect_local_status(2, 'reloaded default block with local DIRECT disabled was not applied')
            # Failed candidate reload retains the working proxy configuration.
            proxy.write_text('invalid = [', encoding='utf-8')
            result = subprocess.run([str(binaries/executable('astra-proxy')), '--reload', str(proxy_control)], capture_output=True, timeout=10)
            assert result.returncode != 0
            assert app.poll() is None
            expect_local_status(2, 'failed proxy reload changed the active policy')
        assert not route.exists(), 'routing endpoint was not cleaned up'
    print('PASS native Windows IPC, Unicode/long paths, SOCKS/HTTP, 64 KiB TCP+EOF, UDP 0..8192, standalone launcher+shutdown, local DIRECT+explicit block+opt-out, reload+rollback')


if __name__ == '__main__':
    main()
