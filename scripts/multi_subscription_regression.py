#!/usr/bin/env python3
"""Core-owned multiple subscriptions, manual trigger and saved URL changes."""
import argparse
import contextlib
import http.server
import json
import os
import pathlib
import subprocess
import tempfile
import threading
import time
from platform_support import executable, process_flags
from protocol_matrix import free_port, wait_port


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = self.server.documents[self.path].encode()
        self.send_response(200)
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = str(args.bin_dir.resolve()/executable('astra-proxy'))
    with tempfile.TemporaryDirectory(prefix='astra multi subscriptions ') as td, contextlib.ExitStack() as stack:
        directory = pathlib.Path(td)
        server = stack.enter_context(http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler))
        def node(name):
            return f'proxies: [{{name: {name}, type: socks5, server: 127.0.0.1, port: 1234}}]'
        server.documents = {'/a': node('a1'), '/b': node('b1'), '/c': node('c1')}
        threading.Thread(target=server.serve_forever, daemon=True).start()
        stack.callback(server.shutdown)
        config = directory/'proxy.toml'
        cache = config.with_suffix('.subscription-cache.json')
        control = directory/'control.sock'
        log_path = directory/'core.log'
        port = free_port()
        url = f'http://127.0.0.1:{server.server_address[1]}'
        base = f'standalone=true\nsocks5_listen="127.0.0.1:{port}"\nshutdown_timeout_secs=1\nwatch_config=true\n'
        base += '[proxy_groups.one]\nsubscription="one"\ninclude_in_proxy=false\n'
        def source(name, path):
            return f'[subscriptions.{name}]\nurl={json.dumps(url+path)}\ninterval=3600\n'
        config.write_text(base+source('one', '/a')+source('two', '/b'), encoding='utf-8')
        with log_path.open('w', encoding='utf-8') as log:
            child = subprocess.Popen([binary, str(config)], stdout=log, stderr=subprocess.STDOUT,
                env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(control)), **process_flags())
            def expect(names):
                deadline = time.monotonic()+20
                while time.monotonic() < deadline:
                    assert child.poll() is None, log_path.read_text(encoding='utf-8')
                    try:
                        import tomllib
                        data = json.loads(cache.read_text(encoding='utf-8'))
                        effective = tomllib.loads(data['effective'])
                        tags = {n['tag'] for n in effective['outbounds']}
                        if tags == set(names):
                            groups = effective['proxy_groups']
                            for source_name, ownership in data['subscriptions'].items():
                                owned = [group for group in groups.values() if group.get('subscription') == source_name]
                                assert len(owned) == 1, groups
                                assert set(owned[0]['members']) == set(ownership['managed_nodes']), owned
                            assert groups['one']['include_in_proxy'] is False
                            return
                    except (OSError, KeyError, ValueError): pass
                    time.sleep(.1)
                raise AssertionError('expected nodes '+repr(names)+'\n'+log_path.read_text(encoding='utf-8'))
            try:
                wait_port(port, child, log_path)
                expect(['a1', 'b1'])
                server.documents['/a'] = node('a2')
                subprocess.run([binary, '--update-subscriptions', str(control)], check=True, capture_output=True)
                expect(['a2', 'b1'])
                server.documents['/b'] = 'invalid YAML configuration'
                server.documents['/a'] = node('a3')
                subprocess.run([binary, '--update-subscriptions', str(control)], check=True, capture_output=True)
                expect(['a3', 'b1'])
                # Saving a changed URL and an added source triggers updates without a manual command.
                server.documents['/b'] = node('b2')
                config.write_text(base+source('one', '/c')+source('two', '/b')+source('three', '/a'), encoding='utf-8')
                expect(['c1', 'b2', 'a3'])
                # Removing a source must remove only its nodes.
                config.write_text(base+source('one', '/c')+source('two', '/b'), encoding='utf-8')
                expect(['c1', 'b2'])
            finally:
                child.terminate()
                child.wait(timeout=15)
        print('PASS multiple subscriptions: startup, manual refresh, independent failure, saved URL/add/remove triggers')


if __name__ == '__main__':
    main()
