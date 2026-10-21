#!/usr/bin/env python3
"""Isolated HTTP Clash subscription: conversion, timed replacement and failed refresh retention."""
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
from proxy_groups_regression import Upstream, Handler
from protocol_matrix import free_port, wait_port
from smoke_test import socks_connect


class Subscription(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.server.requests += 1
        body = self.server.content.encode()
        self.send_response(200)
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    with tempfile.TemporaryDirectory(prefix='astra subscription ') as td, contextlib.ExitStack() as stack:
        directory = pathlib.Path(td)
        upstreams = []
        for marker in ('a', 'b'):
            server = stack.enter_context(Upstream(('127.0.0.1', 0), Handler))
            server.marker, server.delay, server.healthy = marker, 0, True
            threading.Thread(target=server.serve_forever, daemon=True).start()
            stack.callback(server.shutdown)
            upstreams.append(server)
        subscription = stack.enter_context(http.server.ThreadingHTTPServer(('127.0.0.1', 0), Subscription))
        subscription.requests = 0

        def content(index):
            return f"proxies: [{{name: node-{index}, type: http, server: 127.0.0.1, port: {upstreams[index].server_address[1]}}}]\nproxy-groups: [{{name: PROXY, type: select, include-all: true}}, {{name: remote-only, type: select, proxies: [node-{index}]}}]\nrules: ['MATCH,PROXY']\n"

        subscription.content = content(0)
        threading.Thread(target=subscription.serve_forever, daemon=True).start()
        stack.callback(subscription.shutdown)
        url = f'http://127.0.0.1:{subscription.server_address[1]}/config?token=secret'
        subprocess.run([str(binaries/executable('astra-convert')), url, '--interval', '60', '-o', str(directory)], check=True, capture_output=True)
        config = directory/'proxy.toml'
        import tomllib
        parsed = tomllib.loads(config.read_text(encoding='utf-8'))
        assert parsed['subscription']['managed_nodes'] == ['node-0']
        assert parsed['subscription']['interval'] == 60
        assert parsed['subscription']['import_groups'] is False
        assert parsed['subscription']['managed_groups'] == []
        assert not (directory/'rule.toml').exists(), 'node subscription imported rules'
        groups = parsed['proxy_groups']
        owned_groups = [(name, group) for name, group in groups.items()
                        if group.get('subscription') == '__legacy__']
        assert len(owned_groups) == 1, 'node subscription must generate one local subscription group'
        subscription_group_name, subscription_group = owned_groups[0]
        assert set(groups) == {'PROXY', subscription_group_name}, 'node subscription imported remote groups'
        assert subscription_group['members'] == parsed['subscription']['managed_nodes'], 'subscription group membership differs from owned nodes'
        assert subscription_group['include_in_proxy'] is True, 'subscription group must join PROXY by default'
        assert 'remote-only' not in groups, 'nodes-only import must exclude remote groups'
        full = directory/'full'
        subprocess.run([str(binaries/executable('astra-convert')), url, '--include-config', '-o', str(full)], check=True, capture_output=True)
        assert (full/'rule.toml').is_file()
        assert 'remote-only' in tomllib.loads((full/'proxy.toml').read_text(encoding='utf-8'))['proxy_groups']
        initial_requests = subscription.requests
        port = free_port()
        # Keep the generated subscription ownership but isolate all listeners and routing.
        original = config.read_text(encoding='utf-8')
        import re
        original = re.sub(r'(?m)^socks5_listen\s*=.*$', f'socks5_listen = "127.0.0.1:{port}"', original)
        config.write_text('standalone=true\nstandalone_outbound="PROXY"\nshutdown_timeout_secs=1\n'+original, encoding='utf-8')
        saved = config.read_bytes()
        log_path = directory/'core.log'
        with log_path.open('w', encoding='utf-8') as log:
            child = subprocess.Popen([str(binaries/executable('astra-proxy')), str(config)], stdout=log, stderr=subprocess.STDOUT,
                env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(directory/'control.sock')), **process_flags())
            try:
                wait_port(port, child, log_path)

                def probe():
                    stream, status = socks_connect(port, 'example.test', 80)
                    with stream:
                        stream.settimeout(3)
                        assert status == 0
                        stream.sendall(b'GET / HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n')
                        data = bytearray()
                        while chunk := stream.recv(4096):
                            data.extend(chunk)
                        return bytes(data).split(b'\r\n\r\n', 1)[1]

                assert probe() == b'a'
                deadline = time.monotonic()+10
                while subscription.requests <= initial_requests:
                    assert time.monotonic() < deadline, 'startup subscription fetch missing'
                    time.sleep(.1)
                time.sleep(1)
                subscription.content = content(1)
                deadline = time.monotonic()+70
                while probe() != b'b':
                    assert time.monotonic() < deadline, 'timed refresh did not replace node'
                    time.sleep(1)
                print('Timed refresh replaced the node; checking invalid refresh retention.', flush=True)
                subscription.content = '<html>invalid subscription</html>'
                requests = subscription.requests
                deadline = time.monotonic()+70
                while subscription.requests == requests:
                    assert time.monotonic() < deadline, 'next timed refresh missing'
                    time.sleep(1)
                time.sleep(2)
                assert probe() == b'b', 'failed update discarded the working node'
                assert config.read_bytes() == saved, 'subscription overwrote local config'
                assert 'secret' not in log_path.read_text(encoding='utf-8'), 'subscription token leaked to log'
                assert config.with_suffix('.subscription-cache.json').is_file(), 'core did not persist node cache'
                child.terminate()
                child.wait(timeout=15)
                child = subprocess.Popen([str(binaries/executable('astra-proxy')), str(config)], stdout=log, stderr=subprocess.STDOUT,
                    env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(directory/'control.sock')), **process_flags())
                wait_port(port, child, log_path)
                assert probe() == b'b', 'offline restart lost the most recent working nodes'
                child.terminate()
                child.wait(timeout=15)
                # Core can bootstrap directly from its saved URL, with no converted nodes.
                subscription.content = content(1)
                config.write_text(f'standalone=true\nstandalone_outbound="PROXY"\nsocks5_listen="127.0.0.1:{port}"\nshutdown_timeout_secs=1\n[subscription]\nurl={json.dumps(url)}\ninterval=60\n', encoding='utf-8')
                child = subprocess.Popen([str(binaries/executable('astra-proxy')), str(config)], stdout=log, stderr=subprocess.STDOUT,
                    env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(directory/'control.sock')), **process_flags())
                wait_port(port, child, log_path)
                deadline = time.monotonic()+15
                while True:
                    try:
                        if probe() == b'b': break
                    except (OSError, AssertionError):
                        pass
                    assert time.monotonic() < deadline, 'core did not import from its own subscription URL'
                    time.sleep(.2)
            finally:
                child.terminate()
                child.wait(timeout=15)
        print('Subscription regression passed: nodes-only default, timed replacement, rollback, cached offline restart and core-owned URL bootstrap.')


if __name__ == '__main__':
    main()
