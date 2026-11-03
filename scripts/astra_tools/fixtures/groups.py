"""Controllable HTTP CONNECT upstream for subscription and group tests."""
import socketserver
import time

def headers(stream):
    data = bytearray()
    while not data.endswith(b'\r\n\r\n'):
        part = stream.recv(1)
        if not part:
            raise ConnectionError('closed before headers')
        data.extend(part)
        if len(data) > 8192:
            raise ValueError('headers too large')
    return bytes(data)

class Upstream(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True

class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        try:
            self.request.settimeout(5)
            assert headers(self.request).startswith(b'CONNECT example.test:80 ')
            self.request.sendall(b'HTTP/1.1 200 Connection established\r\n\r\n')
            request = headers(self.request)
            probe = request.startswith(b'GET /204 ')
            if probe:
                time.sleep(self.server.delay)
            status = 503 if probe and not self.server.healthy else 200
            body = self.server.marker.encode()
            self.request.sendall(f'HTTP/1.1 {status} Test\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n'.encode() + body)
        except (OSError, AssertionError):
            pass
