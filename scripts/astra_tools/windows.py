"""Explicit Windows integration fixtures. Callers opt in to system changes."""
import contextlib
import ctypes as C
from ctypes import wintypes as W
import http.server
import os
import pathlib
import socket
import socketserver
import ssl
import subprocess
import threading
import time
from urllib.parse import urlsplit
from astra_tools.platform import process_flags
from astra_tools.net import exact, free_port
from astra_tools.workspace import ROOT

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
                                      env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(self.control), ASTRA_LOG_LEVEL=getattr(args,'log_level','debug')),
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

def powershell(script):
    result = subprocess.run(['powershell.exe','-NoProfile','-NonInteractive','-Command', script], capture_output=True, text=True, timeout=20)
    if result.returncode:
        raise RuntimeError(result.stderr)
    return result.stdout.strip()
