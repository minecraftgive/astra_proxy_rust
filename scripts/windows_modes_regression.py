#!/usr/bin/env python3
"""Real WinINet and Wintun integration tests; restore owned system state on exit."""
import argparse
import contextlib
import ctypes as C
from ctypes import wintypes as W
import http.server
import json
import os
import pathlib
import socket
import socketserver
import ssl
import subprocess
import threading
import time
import traceback
from urllib.parse import urlsplit
from platform_support import process_flags
from protocol_matrix import exact, free_port

ROOT = pathlib.Path(__file__).resolve().parents[1]
WIN = C.WinDLL('wininet', use_last_error=True) if os.name == 'nt' else None


class Value(C.Union):
    _fields_ = [('number', W.DWORD), ('text', C.c_void_p), ('time', W.FILETIME)]


class Option(C.Structure):
    _fields_ = [('key', W.DWORD), ('value', Value)]


class Options(C.Structure):
    _fields_ = [('size', W.DWORD), ('connection', W.LPWSTR), ('count', W.DWORD),
                ('error', W.DWORD), ('options', C.POINTER(Option))]


def proxy_state():
    values = (Option * 4)()
    for item, key in zip(values, (10, 2, 3, 4)):
        item.key = key
    options = Options(C.sizeof(Options), None, 4, 0, values)
    size = W.DWORD(C.sizeof(options))
    if not WIN.InternetQueryOptionW(None, 75, C.byref(options), C.byref(size)):
        raise C.WinError(C.get_last_error())
    strings = []
    kernel = C.WinDLL('kernel32')
    kernel.GlobalFree.argtypes = [C.c_void_p]
    for item in values[1:]:
        strings.append(C.wstring_at(item.value.text) if item.value.text else '')
        if item.value.text:
            kernel.GlobalFree(item.value.text)
    return dict(flags=values[0].value.number, server=strings[0], bypass=strings[1], pac=strings[2])


def wininet_get(url):
    # PRECONFIG uses the actual current-user system proxy, without an explicit proxy argument.
    WIN.InternetOpenW.restype = C.c_void_p
    WIN.InternetOpenW.argtypes = [W.LPCWSTR, W.DWORD, W.LPCWSTR, W.LPCWSTR, W.DWORD]
    WIN.InternetOpenUrlW.restype = C.c_void_p
    WIN.InternetOpenUrlW.argtypes = [C.c_void_p, W.LPCWSTR, W.LPCWSTR, W.DWORD, W.DWORD, C.c_size_t]
    WIN.InternetReadFile.argtypes = [C.c_void_p, C.c_void_p, W.DWORD, C.POINTER(W.DWORD)]
    WIN.InternetCloseHandle.argtypes = [C.c_void_p]
    session = WIN.InternetOpenW('Astra integration test', 0, None, None, 0)
    if not session:
        raise C.WinError(C.get_last_error())
    request = connection = None
    try:
        # Bound network waits even on a regression.
        for option in (2, 5, 6):
            value = W.DWORD(5000)
            WIN.InternetSetOptionW(C.c_void_p(session), option, C.byref(value), 4)
        flags = 0x80000000 | 0x04000000 | 0x200
        parsed = urlsplit(url)
        if parsed.scheme == 'https':
            WIN.InternetConnectW.restype = C.c_void_p
            WIN.InternetConnectW.argtypes = [C.c_void_p,W.LPCWSTR,W.WORD,W.LPCWSTR,W.LPCWSTR,W.DWORD,W.DWORD,C.c_size_t]
            WIN.HttpOpenRequestW.restype = C.c_void_p
            WIN.HttpOpenRequestW.argtypes = [C.c_void_p,W.LPCWSTR,W.LPCWSTR,W.LPCWSTR,W.LPCWSTR,C.c_void_p,W.DWORD,C.c_size_t]
            WIN.HttpSendRequestW.argtypes = [C.c_void_p,W.LPCWSTR,W.DWORD,C.c_void_p,W.DWORD]
            connection = WIN.InternetConnectW(session,parsed.hostname,parsed.port,None,None,3,0,0)
            if not connection:
                raise C.WinError(C.get_last_error())
            request = WIN.HttpOpenRequestW(connection,'GET',parsed.path,None,None,None,flags | 0x800000 | 0x1000 | 0x2000,0)
            if not request:
                raise C.WinError(C.get_last_error())
            # Only this throwaway local HTTPS request accepts our self-signed certificate.
            security = W.DWORD(0x100 | 0x1000 | 0x2000)
            if not WIN.InternetSetOptionW(C.c_void_p(request),31,C.byref(security),4):
                raise C.WinError(C.get_last_error())
            if not WIN.HttpSendRequestW(request,None,0,None,0):
                raise C.WinError(C.get_last_error())
        else:
            request = WIN.InternetOpenUrlW(session, url, None, 0, flags, 0)
        if not request:
            raise C.WinError(C.get_last_error())
        result = bytearray()
        while True:
            buffer = C.create_string_buffer(8192)
            count = W.DWORD()
            if not WIN.InternetReadFile(request, buffer, len(buffer), C.byref(count)):
                raise C.WinError(C.get_last_error())
            if not count.value:
                return bytes(result)
            result.extend(buffer.raw[:count.value])
    finally:
        if request:
            WIN.InternetCloseHandle(request)
        if connection:
            WIN.InternetCloseHandle(connection)
        WIN.InternetCloseHandle(session)


class SocksHandler(socketserver.BaseRequestHandler):
    def handle(self):
        stream = self.request
        stream.settimeout(15)
        try:
            version, n = exact(stream, 2)
            assert version == 5
            exact(stream, n)
            stream.sendall(b'\x05\x00')
            version, command, _, family = exact(stream, 4)
            if family == 1:
                host = socket.inet_ntop(socket.AF_INET, exact(stream, 4))
            elif family == 4:
                host = socket.inet_ntop(socket.AF_INET6, exact(stream, 16))
            else:
                host = exact(stream, exact(stream, 1)[0]).decode()
            port = int.from_bytes(exact(stream, 2), 'big')
            self.server.hits.append((command, host, port))
            if command == 3:
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
                    udp.bind(('127.0.0.1', 0))
                    udp.settimeout(.2)
                    stream.sendall(b'\x05\x00\x00\x01\x7f\x00\x00\x01' + udp.getsockname()[1].to_bytes(2, 'big'))
                    while not self.server.done.is_set():
                        try:
                            data, peer = udp.recvfrom(65535)
                            udp.sendto(data, peer)
                        except socket.timeout:
                            continue
            elif command == 1:
                stream.sendall(b'\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00')
                if self.server.echo:
                    while data := stream.recv(65536):
                        stream.sendall(data)
                else:
                    with socket.create_connection(('127.0.0.1', port), timeout=5) as origin:
                        def upload():
                            try:
                                while data := stream.recv(65536):
                                    origin.sendall(data)
                                origin.shutdown(socket.SHUT_WR)
                            except OSError:
                                pass
                        threading.Thread(target=upload, daemon=True).start()
                        while data := origin.recv(65536):
                            stream.sendall(data)
        except (OSError, EOFError):
            pass


class SocksServer(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


@contextlib.contextmanager
def upstream(echo):
    with SocksServer(('127.0.0.1', 0), SocksHandler) as server:
        server.echo, server.hits, server.done = echo, [], threading.Event()
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            yield server
        finally:
            server.done.set()
            server.shutdown()


class Origin(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'astra-system-proxy:' + self.path.encode() + bytes(range(256)) * 256
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class Core:
    def __init__(self, args, directory, config, name):
        self.binary = str(args.bin_dir/'astra-proxy.exe')
        self.control = directory/(name+f'-{os.getpid()}.ctl')
        self.journal = self.control.with_suffix('.system-proxy-state.toml')
        self.config = directory/(name+'.toml')
        self.config.write_text(config, encoding='utf-8')
        self.log_path = directory/(name+'.log')
        self.log = self.log_path.open('w', encoding='utf-8')
        self.child = subprocess.Popen([self.binary, str(self.config)], stdout=self.log, stderr=subprocess.STDOUT,
                                      env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(self.control), ASTRA_LOG_LEVEL='debug'),
                                      **process_flags())

    def ready(self):
        deadline = time.monotonic()+30
        while not self.control.exists():
            if self.child.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError(self.log_path.read_text(encoding='utf-8'))
            time.sleep(.1)

    def command(self, action):
        return subprocess.run([self.binary, action, str(self.control)], capture_output=True, text=True, timeout=15)

    def close(self):
        try:
            if self.child.poll() is None:
                result = self.command('--stop')
                if result.returncode:
                    raise RuntimeError(result.stderr)
                self.child.wait(timeout=20)
        finally:
            if self.child.poll() is None:
                self.child.kill()
                self.child.wait(timeout=10)
            if self.journal.exists():
                subprocess.run([self.binary, '--restore-system-proxy', str(self.journal)], check=True, timeout=15)
            # The forced-termination test owns this marker; its process is now gone.
            self.control.unlink(missing_ok=True)
            self.log.close()


def base(server, port):
    return f'''standalone=true
socks5_listen="127.0.0.1:{port}"
shutdown_timeout_secs=1
[[outbounds]]
kind="socks5"
tag="local-test-upstream"
server="127.0.0.1:{server.server_address[1]}"
'''


def system_proxy(args, directory):
    original = proxy_state()
    checks = []
    with upstream(False) as server, http.server.ThreadingHTTPServer(('127.0.0.1', 0), Origin) as origin:
        threading.Thread(target=origin.serve_forever, daemon=True).start()
        port, http_port = free_port(), free_port()
        config = base(server, port).replace('shutdown_timeout_secs=1', f'http_listen="127.0.0.1:{http_port}"\nshutdown_timeout_secs=1')+'\n[system_proxy]\n'
        core = Core(args, directory, config, 'system-proxy')
        try:
            core.ready()
            active = proxy_state()
            assert active['flags'] == 3 and active['server'] == f'http=127.0.0.1:{http_port};https=127.0.0.1:{http_port}', active
            assert not active['pac'] and core.journal.exists()
            body = wininet_get(f'http://system-proxy.astra.invalid:{origin.server_port}/wininet')
            assert body == b'astra-system-proxy:/wininet'+bytes(range(256))*256
            assert (1, 'system-proxy.astra.invalid', origin.server_port) in server.hits
            checks.append('actual WinINet PRECONFIG HTTP, 64 KiB streaming, takeover settings')
            certs = directory/'tls'
            if not certs.exists():
                subprocess.run([str(args.bin_dir/'astra-server-core.exe'),'--generate-cert',str(certs),'localhost'],check=True,capture_output=True,timeout=10)
            with http.server.ThreadingHTTPServer(('127.0.0.1',0),Origin) as secure:
                context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
                context.load_cert_chain(certs/'cert.pem',certs/'key.pem')
                secure.socket = context.wrap_socket(secure.socket,server_side=True)
                threading.Thread(target=secure.serve_forever,daemon=True).start()
                try:
                    body = wininet_get(f'https://system-proxy.astra.invalid:{secure.server_port}/https')
                    assert body == b'astra-system-proxy:/https'+bytes(range(256))*256
                    assert (1,'system-proxy.astra.invalid',secure.server_port) in server.hits
                finally:
                    secure.shutdown()
            checks.append('actual WinINet PRECONFIG HTTPS through CONNECT, 64 KiB TLS transfer')
            # Dedicated CONNECT tunnel traverses the same selected upstream.
            with socket.create_connection(('127.0.0.1', http_port), timeout=5) as stream:
                stream.sendall(f'CONNECT system-proxy.astra.invalid:{origin.server_port} HTTP/1.1\r\nHost: system-proxy.astra.invalid\r\n\r\n'.encode())
                response = bytearray()
                while not response.endswith(b'\r\n\r\n'):
                    response.extend(exact(stream, 1))
                assert response.startswith(b'HTTP/1.1 200'), response
                stream.sendall(b'GET /connect HTTP/1.0\r\nHost: test\r\n\r\n')
                assert b'200 OK' in stream.recv(1024)
            checks.append('CONNECT tunnel')
            contender = Core(args, directory, config.replace(str(port), str(free_port()), 1).replace(f':{http_port}', f':{free_port()}'), 'system-proxy-contender')
            try:
                assert contender.child.wait(timeout=15) != 0
                assert proxy_state() == active
            finally:
                contender.close()
            checks.append('exclusive takeover ownership')
            core.config.write_text(config.replace('[system_proxy]',''),encoding='utf-8')
            assert core.command('--reload').returncode == 0
            assert proxy_state() == original and not core.journal.exists()
            core.config.write_text(config,encoding='utf-8')
            assert core.command('--reload').returncode == 0
            assert proxy_state() == active
            checks.append('hot reload disables/restores and re-enables takeover')
        finally:
            core.close()
            origin.shutdown()
        assert proxy_state() == original and not core.journal.exists(), 'normal shutdown did not restore settings'
        checks.append('normal shutdown restores original proxy/PAC/bypass/flags')
        core = Core(args, directory, config, 'system-proxy-crash')
        try:
            core.ready()
            core.child.kill()
            core.child.wait(timeout=10)
            assert core.journal.exists()
        finally:
            core.close()
        assert proxy_state() == original and not core.journal.exists(), 'crash recovery did not restore settings'
        checks.append('forced termination and durable recovery')
    return checks


def powershell(script):
    result = subprocess.run(['powershell.exe','-NoProfile','-NonInteractive','-Command', script], capture_output=True, text=True, timeout=20)
    if result.returncode:
        raise RuntimeError(result.stderr)
    return result.stdout.strip()


def tun(args, directory):
    if not C.windll.shell32.IsUserAnAdmin():
        raise PermissionError('Wintun integration test requires an elevated process')
    interface = args.interface
    if not interface or not interface.isdigit():
        raise ValueError('--interface must be a physical interface index')
    targets = ('198.19.253.2', 'fd00:198:19:253::2')
    prefixes = (targets[0]+'/32', targets[1]+'/128')
    query = "@(Get-NetRoute -ErrorAction SilentlyContinue | Where-Object { $_.DestinationPrefix -in @('"+"','".join(prefixes)+"') }).Count"
    assert powershell(query) == '0', 'test prefixes already routed'
    name = 'AstraTest'+str(os.getpid())
    checks = []
    with upstream(True) as server:
        config = base(server, free_port())+f'''
[tun]
name="{name}"
address="198.18.253.1"
prefix=30
ipv6_address="fd00:198:18:253::1"
ipv6_prefix=126
mtu=1500
interface="{interface}"
routes={json.dumps(list(prefixes))}
'''
        core = Core(args, directory, config, 'tun')
        try:
            core.ready()
            assert powershell(query) == '2'
            checks.append('signed Wintun adapter creation and IPv4/IPv6 owned host routes')
            for host in targets:
                for n in (1, 4096, 65536, 1048576):
                    with socket.create_connection((host, 18081), timeout=10) as stream:
                        payload = bytes([n % 251])*n
                        stream.sendall(payload)
                        assert exact(stream,n) == payload
                        stream.shutdown(socket.SHUT_WR)
                        assert stream.recv(1) == b''
                family = socket.AF_INET6 if ':' in host else socket.AF_INET
                with socket.socket(family, socket.SOCK_DGRAM) as udp:
                    udp.settimeout(5)
                    for n in (0, 1, 64, 1200):
                        data = bytes([n % 251])*n
                        udp.sendto(data, (host, 18082))
                        received, peer = udp.recvfrom(65535)
                        assert received == data and peer[0] == host and peer[1] == 18082
                checks.append(f'{host}: native TCP 1..1MiB + EOF; UDP 0..1200')
                print('PASS',checks[-1],flush=True)
            assert core.command('--reload').returncode == 0
            assert powershell(query) == '2'
            with socket.create_connection((targets[0], 18081), timeout=5) as stream:
                stream.sendall(b'reloaded')
                assert exact(stream,8) == b'reloaded'
            checks.append('same-adapter reload preserves owned routes and traffic')
        finally:
            core.close()
        assert powershell(query) == '0', 'owned routes leaked'
        assert powershell(f"@(Get-NetAdapter -Name '{name}' -ErrorAction SilentlyContinue).Count") == '0', 'test adapter leaked'
        checks.append('stop removes owned routes and test adapter')
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mode', choices=['system-proxy','tun'], required=True)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT/'target/debug')
    parser.add_argument('--output-dir', type=pathlib.Path, required=True)
    parser.add_argument('--interface')
    args = parser.parse_args()
    args.bin_dir = args.bin_dir.resolve()
    directory = args.output_dir.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    report = {'mode':args.mode, 'passed':False}
    try:
        report['checks'] = (system_proxy if args.mode == 'system-proxy' else tun)(args,directory)
        report['passed'] = True
        print(json.dumps(report,indent=2),flush=True)
    except Exception:
        report['error'] = traceback.format_exc()
        raise
    finally:
        (directory/'result.json').write_text(json.dumps(report,indent=2)+'\n',encoding='utf-8')


if __name__ == '__main__':
    main()
