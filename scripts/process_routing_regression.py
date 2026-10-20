#!/usr/bin/env python3
"""Real local process rules and LAN fallback, without changing system networking."""
import argparse
import contextlib
import http.server
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from platform_support import executable, process_flags
from protocol_matrix import free_port, wait_port, UdpEchoServer, UdpEcho, udp_probe
from smoke_test import socks_connect

class Server(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Length', '2')
        self.end_headers()
        self.wfile.write(b'ok')
    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path, required=True)
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    curl = shutil.which('curl.exe' if os.name == 'nt' else 'curl')
    assert curl, 'curl is required for a second application identity'
    with tempfile.TemporaryDirectory(prefix='astra-process-') as temporary, contextlib.ExitStack() as stack:
        root = Path(temporary)
        server = stack.enter_context(http.server.ThreadingHTTPServer(('127.0.0.1', 0), Server))
        threading.Thread(target=server.serve_forever, daemon=True).start()
        stack.callback(server.shutdown)
        port = server.server_address[1]
        udp = stack.enter_context(UdpEchoServer(('127.0.0.1', 0), UdpEcho))
        threading.Thread(target=udp.serve_forever, daemon=True).start()
        stack.callback(udp.shutdown)
        socks, http_port = free_port(), free_port()
        route = root / 'route.sock'
        rules, config = root/'rule.toml', root/'proxy.toml'
        config.write_text(f'rule_socket={json.dumps(route.as_posix())}\nsocks5_listen="127.0.0.1:{socks}"\nhttp_listen="127.0.0.1:{http_port}"\nshutdown_timeout_secs=1\n', encoding='utf-8')
        def write_rules(kind='process_name', value=None, direct=True):
            if value is None:
                value = Path(sys.executable).name
            rules.write_text(f'socket={json.dumps(route.as_posix())}\ndefault_action="block"\ndirect_local={str(direct).lower()}\n[[rules]]\nkind={json.dumps(kind)}\nvalue={json.dumps(value)}\naction="block"\n', encoding='utf-8')
        write_rules()
        children = []
        for name, path in [('astra-rule', rules), ('astra-proxy', config)]:
            if name == 'astra-proxy':
                deadline = time.monotonic()+10
                while not route.exists():
                    assert children[0][0].poll() is None and time.monotonic() < deadline
                    time.sleep(.02)
            log = stack.enter_context((root/f'{name}.log').open('w',encoding='utf-8'))
            endpoint = root/f'{name}.ctl'
            binary = str(binaries/executable(name))
            child = subprocess.Popen([binary,str(path)],stdout=log,stderr=subprocess.STDOUT,env=dict(os.environ,ASTRA_CONTROL_SOCKET=str(endpoint),ASTRA_LOG_LEVEL='route'),**process_flags())
            children.append((child,binary,endpoint))
            def cleanup(child=child,binary=binary,endpoint=endpoint):
                if child.poll() is None:
                    stop = subprocess.run([binary,'--stop',str(endpoint)],capture_output=True,timeout=10)
                    if stop.returncode:
                        child.kill()
                child.wait(timeout=15)
            stack.callback(cleanup)
        proxy_log = root/'astra-proxy.log'
        wait_port(socks,children[1][0],proxy_log)
        def python_blocked():
            client,status = socks_connect(socks,'127.0.0.1',port)
            client.close()
            assert status == 2, (status,proxy_log.read_text(encoding='utf-8'))
            for verb in [f'GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n',f'CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n']:
                with socket.create_connection(('127.0.0.1',http_port),timeout=5) as client:
                    client.sendall(verb.encode())
                    assert b' 403 ' in client.recv(4096).split(b'\r\n',1)[0]
        def curl_allowed(socks_mode=False):
            option = ['--socks5-hostname',f'127.0.0.1:{socks}'] if socks_mode else ['--proxy',f'http://127.0.0.1:{http_port}']
            result = subprocess.run([curl,'--silent','--show-error','--fail','--max-time','5','--noproxy','',*option,f'http://127.0.0.1:{port}/'],capture_output=True,timeout=10)
            assert result.returncode == 0 and result.stdout == b'ok', (result,proxy_log.read_text(encoding='utf-8'))
        # Alternating applications share a destination and cache generation.
        for _ in range(2):
            python_blocked()
            curl_allowed()
            curl_allowed(True)
        try:
            udp_probe(socks, udp.server_address[1], sizes=[17], hosts=['127.0.0.1'])
        except (TimeoutError, socket.timeout):
            pass
        else:
            raise AssertionError('Process-blocked UDP unexpectedly reached its destination')
        def reload():
            child,binary,endpoint=children[0]
            subprocess.run([binary,'--reload',str(endpoint)],check=True,capture_output=True,timeout=10)
            time.sleep(.2)  # Allow the revision subscriber to observe the commit.
        write_rules('process_path',str(Path(sys.executable).resolve()))
        reload()
        python_blocked()
        curl_allowed()
        write_rules('process_name', 'no-such-application-for-test')
        reload()
        udp_probe(socks, udp.server_address[1], sizes=[0,17], hosts=['127.0.0.1'])
        # Disabling the fallback restores the configured default BLOCK for curl.
        write_rules('process_path',str(Path(sys.executable).resolve()),False)
        reload()
        result = subprocess.run([curl,'--silent','--fail','--max-time','5','--noproxy','','--proxy',f'http://127.0.0.1:{http_port}',f'http://127.0.0.1:{port}/'],capture_output=True,timeout=10)
        assert result.returncode == 22
        text=proxy_log.read_text(encoding='utf-8')
        assert 'process=Some(ProcessInfo' in text
        assert Path(sys.executable).name.lower() in text.lower()
    print('PASS process name/path, SOCKS/HTTP/CONNECT/UDP attribution, cross-application cache isolation, local DIRECT fallback, reload and opt-out')

if __name__ == '__main__':
    main()
