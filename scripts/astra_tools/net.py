"""Local network fixtures and protocol probes shared by regression scenarios."""
import random
import socket
import socketserver
import ssl

TRAILER = b'astra-half-close-trailer'

class Echo(socketserver.BaseRequestHandler):
    def handle(self):
        while True:
            data = self.request.recv(65536)
            if not data:
                if getattr(self.server, "send_trailer", False):
                    self.request.sendall(TRAILER)
                return
            self.request.sendall(data)

class TlsEcho(socketserver.BaseRequestHandler):
    def handle(self):
        try:
            with self.server.tls.wrap_socket(self.request, server_side=True) as stream:
                while True:
                    data = stream.recv(65536)
                    if not data:
                        stream.unwrap().close()
                        return
                    stream.sendall(data)
        except Exception as error:
            self.server.errors.append(error)

def tls_probe(proxy_port, echo_port, payload_bytes, ca):
    context = ssl.create_default_context(cafile=str(ca))
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    with socket.create_connection(('127.0.0.1', proxy_port), timeout=20) as raw:
        raw.sendall(b'\x05\x01\x00')
        assert exact(raw, 2) == b'\x05\x00'
        raw.sendall(b'\x05\x01\x00\x03\x09localhost' + echo_port.to_bytes(2, 'big'))
        assert exact(raw, 10)[1] == 0
        with context.wrap_socket(raw, server_hostname='localhost') as stream:
            assert stream.version() == 'TLSv1.3'
            # Alternate bounded writes/reads so Python's TLS object is never
            # accessed concurrently by two threads. Large TLS records exercise
            # the Vision switch beyond its initial padded handshake records.
            remaining = payload_bytes
            while remaining:
                payload = bytes(i % 251 for i in range(min(32768, remaining)))
                stream.sendall(payload)
                assert exact(stream, len(payload)) == payload
                remaining -= len(payload)
            stream.unwrap().close()

class UdpEcho(socketserver.BaseRequestHandler):
    def handle(self):
        data, sock = self.request
        sock.sendto(data, self.client_address)

class UdpEchoServer(socketserver.ThreadingUDPServer):
    allow_reuse_address = True
    daemon_threads = True

class UdpEchoServer6(UdpEchoServer):
    address_family = socket.AF_INET6

def udp_probe(proxy_port, echo_port, edge=False, sizes=None, hosts=None):
    with socket.create_connection(('127.0.0.1', proxy_port), timeout=15) as control, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
        udp.bind(('127.0.0.1', 0))
        # First packet includes route lookup and the configured 10-second
        # protocol handshake (REALITY can exceed five seconds locally).
        udp.settimeout(15)
        control.sendall(b'\x05\x01\x00')
        assert exact(control, 2) == b'\x05\x00'
        control.sendall(b'\x05\x03\x00\x01' + socket.inet_aton('127.0.0.1') + udp.getsockname()[1].to_bytes(2, 'big'))
        reply = exact(control, 10)
        assert reply[:4] == b'\x05\x00\x00\x01', reply
        relay = (socket.inet_ntoa(reply[4:8]), int.from_bytes(reply[8:], 'big'))
        for host in (hosts if hosts is not None else ('127.0.0.1', '::1', 'localhost')):
            address = b'\x04' + socket.inet_pton(socket.AF_INET6, host) if ':' in host else (b'\x01' + socket.inet_aton(host) if host[0].isdigit() else b'\x03' + bytes([len(host)]) + host.encode())
            header = b'\x00\x00\x00' + address + echo_port.to_bytes(2, 'big')
            for length in (sizes if sizes is not None else ((1, 1200, 4096, 8192, 0, 64) if edge else (1, 1200, 4096, 64))):
                payload = bytes(i % 251 for i in range(length))
                udp.sendto(header + payload, relay)
                try:
                    data, _ = udp.recvfrom(65536)
                except (socket.timeout, TimeoutError) as error:
                    raise TimeoutError(f"UDP {host} payload={length} timed out") from error
                assert data[:3] == b'\x00\x00\x00', data[:10]
                at = {1: 8, 4: 20}.get(data[3], 5 + data[4])
                assert int.from_bytes(data[at:at+2], 'big') == echo_port
                assert data[at+2:] == payload, (host, length, len(data))

class EchoServer(socketserver.ThreadingTCPServer):
    request_queue_size = 128
    allow_reuse_address = True
    daemon_threads = True

def free_port():
    # Servers often listen on TCP and UDP with one port; Windows reserves different
    # ranges per protocol (netsh int ipv4 show excludedportrange), so check both.
    # Ports are drawn at random: the OS hands out ephemeral TCP ports in sequence and
    # can sit inside a hundreds-wide UDP exclusion for many consecutive picks.
    for _ in range(500):
        port = random.randint(20000, 60999)
        with socket.socket() as tcp, socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
            try:
                tcp.bind(('127.0.0.1', port))
                udp.bind(('127.0.0.1', port))
            except OSError:
                continue
            return port
    raise RuntimeError('no port is free for both TCP and UDP')

def exact(sock, count):
    data = b''
    while len(data) < count:
        chunk = sock.recv(count - len(data))
        if not chunk:
            raise EOFError(f'unexpected EOF after {len(data)} of {count} bytes')
        data += chunk
    return data


def socks_connect(port, host, dest_port, *, timeout=5, username=None, password=None):
    conn = socket.create_connection(('127.0.0.1', port), timeout=timeout)
    try:
        method = 2 if username is not None else 0
        conn.sendall(bytes([5, 1, method]))
        assert exact(conn, 2) == bytes([5, method])
        if method == 2:
            user, secret = username.encode(), (password or '').encode()
            if not 0 < len(user) <= 255 or not 0 < len(secret) <= 255:
                raise ValueError('SOCKS credentials must fit one byte')
            conn.sendall(bytes([1, len(user)]) + user + bytes([len(secret)]) + secret)
            assert exact(conn, 2) == b'\x01\x00'
        encoded = host.encode('idna')
        if not 0 < len(encoded) <= 255:
            raise ValueError('SOCKS destination name must fit one byte')
        conn.sendall(b'\x05\x01\x00\x03' + bytes([len(encoded)]) + encoded + dest_port.to_bytes(2, 'big'))
        header = exact(conn, 4)
        assert header[0] == 5 and header[2] == 0, header
        if header[3] == 1:
            length = 4
        elif header[3] == 4:
            length = 16
        elif header[3] == 3:
            length = exact(conn, 1)[0]
        else:
            raise ValueError('Invalid SOCKS reply address type')
        exact(conn, length + 2)
        return conn, header[1]
    except BaseException:
        conn.close()
        raise


def socks(port, target_port):
    # Keep the explicit IPv4 request used by the admission/routing regressions.
    conn = socket.create_connection(('127.0.0.1', port), timeout=3)
    try:
        conn.sendall(b'\x05\x01\x00')
        assert exact(conn, 2) == b'\x05\x00'
        conn.sendall(b'\x05\x01\x00\x01\x7f\x00\x00\x01' + target_port.to_bytes(2, 'big'))
        return conn, exact(conn, 10)[1]
    except BaseException:
        conn.close()
        raise
