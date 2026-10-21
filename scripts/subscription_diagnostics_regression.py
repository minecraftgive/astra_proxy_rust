#!/usr/bin/env python3
"""Verify actionable subscription failures and credential redaction in real core logs."""
import argparse
import http.server
import os
import pathlib
import subprocess
import tempfile
import threading
import time
from platform_support import executable, process_flags
from protocol_matrix import free_port


class Subscription(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(self.server.status)
        self.end_headers()
        self.wfile.write(self.server.body.encode())

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, required=True)
    args = parser.parse_args()
    binary = str(args.bin_dir.resolve()/executable('astra-proxy'))
    with tempfile.TemporaryDirectory(prefix='astra subscription diagnostics ') as td, http.server.ThreadingHTTPServer(('127.0.0.1', 0), Subscription) as server:
        directory = pathlib.Path(td)
        server.status, server.body = 403, ''
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            config = directory/'proxy.toml'
            control = directory/'proxy.ctl'
            text = f"standalone=true\nsocks5_listen='127.0.0.1:{free_port()}'\nshutdown_timeout_secs=1\n[subscriptions.no]\nurl='http://127.0.0.1:{server.server_port}/private-secret?token=query-secret'\ninterval=60\n"
            config.write_text(text, encoding='utf-8')
            log_path = directory/'proxy.log'
            with log_path.open('w', encoding='utf-8') as log:
                child = subprocess.Popen([binary, str(config)], env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(control)), stdout=log, stderr=subprocess.STDOUT, **process_flags())
                try:
                    deadline = time.monotonic()+15
                    while True:
                        output = log_path.read_text(encoding='utf-8')
                        if 'subscription download or validation failed' in output:
                            break
                        if child.poll() is not None or time.monotonic() >= deadline:
                            raise AssertionError(output)
                        time.sleep(.05)
                    for expected in ['subscription=no', 'HTTP 403', 'download subscription', 'endpoint=', 'elapsed_ms=', 'retry_secs=60', 'previous nodes retained']:
                        assert expected in output, output
                    assert 'private-secret' not in output and 'query-secret' not in output, output
                finally:
                    if child.poll() is None:
                        subprocess.run([binary, '--stop', str(control)], check=True, timeout=10, capture_output=True)
                        child.wait(timeout=15)
                    assert child.returncode == 0, log_path.read_text(encoding='utf-8')
            for status, body, expected in [
                (403, '', 'HTTP 403'),
                (200, '<html>body-secret</html>', 'HTML/XML'),
                (200, 'proxies: [', 'YAML at line'),
                (200, 'proxies: [{name: invalid, type: imaginary, server: 127.0.0.1, port: 1234, password: body-secret}]', 'imaginary'),
            ]:
                server.status, server.body = status, body
                result = subprocess.run([binary, '--refresh-subscriptions-stdin', str(config)], input=text, text=True, encoding='utf-8', capture_output=True, timeout=30)
                assert result.returncode != 0 and expected in result.stderr, result.stderr
                assert all(secret not in result.stderr for secret in ['private-secret', 'query-secret', 'body-secret']), result.stderr
                assert config.read_text(encoding='utf-8') == text
        finally:
            server.shutdown()
    print('PASS subscription diagnostics: live HTTP status/error chain/timing/retry, stopped updates, HTML/YAML/unsupported protocol errors, credential redaction')


if __name__ == '__main__':
    main()
