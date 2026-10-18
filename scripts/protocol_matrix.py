#!/usr/bin/env python3
"""Local payload interoperability tests using real protocol servers, without public nodes."""
import argparse
import base64
import contextlib
import json
import os
import pathlib
import signal
import socket
import socketserver
import ssl
import subprocess
import tempfile
import threading
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]
USER = '00000000-0000-0000-0000-000000000001'
PASSWORD = 'astra-local-test-password'
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
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]

def exact(sock, count):
    data = b''
    while len(data) < count:
        chunk = sock.recv(count - len(data))
        if not chunk:
            raise EOFError(f'unexpected EOF after {len(data)} of {count} bytes')
        data += chunk
    return data

def wait_port(port, process, log):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError('process exited: ' + log.read_text()[-5000:])
        try:
            with socket.create_connection(('127.0.0.1', port), timeout=.1):
                return
        except OSError:
            time.sleep(.05)
    raise TimeoutError('listener startup timeout: ' + log.read_text()[-5000:])

@contextlib.contextmanager
def process(command, directory, name):
    log_path = directory / (name + '.log')
    with log_path.open('w') as log:
        child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, cwd=ROOT,
                                 env=dict(os.environ, RUST_LOG=os.environ.get('RUST_LOG', 'info')))
        try:
            yield child, log_path
        finally:
            if child.poll() is None:
                child.send_signal(signal.SIGTERM)
                try:
                    child.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()

def write_json(path, data):
    path.write_text(json.dumps(data, indent=2))
    return path

def tls_material(directory):
    cert, key = directory / 'cert.pem', directory / 'key.pem'
    ca, ca_key = directory / 'ca.pem', directory / 'ca-key.pem'
    config = directory / 'openssl.cnf'
    config.write_text('[req]\ndistinguished_name=dn\nx509_extensions=ca\nprompt=no\n[dn]\nCN=Astra Test CA\n[ca]\nbasicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,cRLSign\n[server]\nsubjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n')
    commands = [
        ['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '2', '-config', str(config), '-keyout', str(ca_key), '-out', str(ca)],
        ['openssl', 'req', '-newkey', 'rsa:2048', '-nodes', '-subj', '/CN=localhost', '-keyout', str(key), '-out', str(directory/'server.csr')],
        ['openssl', 'x509', '-req', '-in', str(directory/'server.csr'), '-CA', str(ca), '-CAkey', str(ca_key), '-CAcreateserial', '-days', '2', '-extfile', str(config), '-extensions', 'server', '-out', str(cert)],
    ]
    for command in commands:
        subprocess.run(command, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return cert, key

def fixture(case, port, cert, key):
    if case.startswith('sing-socks-'):
        inbound = {'type': 'socks', 'listen': '127.0.0.1', 'listen_port': port}
        if case.endswith('-auth'):
            inbound['users'] = [{'username': 'astra', 'password': PASSWORD}]
        return 'sing_box', {'log': {'level':'warn'}, 'inbounds':[inbound], 'outbounds':[{'type':'direct'}]}, None
    if case.startswith(('socks-', 'http-')):
        protocol, authentication = case.split('-', 1)
        settings = {'udp': True, 'auth': 'password' if authentication == 'auth' else 'noauth'} if protocol == 'socks' else {}
        if authentication == 'auth':
            settings['accounts'] = [{'user': 'astra', 'pass': PASSWORD}]
        return 'xray', {'log': {'loglevel': 'warning'}, 'inbounds': [{'listen': '127.0.0.1', 'port': port, 'protocol': protocol, 'settings': settings}], 'outbounds': [{'protocol': 'freedom'}]}, None
    if case.startswith('sing-'):
        protocol = case[5:].removesuffix('-aead')
        if protocol == 'direct':
            return 'sing_box', None, {'type': 'direct'}
        server = {'type': protocol, 'listen': '127.0.0.1', 'listen_port': port}
        client = {'type': protocol, 'server': '127.0.0.1', 'server_port': port}
        tls_server = {'enabled': True, 'certificate_path': str(cert), 'key_path': str(key), 'alpn': ['h3']}
        tls_client = {'enabled': True, 'server_name': 'localhost', 'certificate_path': str(cert), 'alpn': ['h3']}
        if protocol == 'hysteria2':
            server.update(users=[{'password': PASSWORD}], tls=tls_server)
            client.update(password=PASSWORD, tls=tls_client)
        elif protocol in ('vmess', 'vless'):
            tls_server['alpn'] = ['http/1.1']
            user = {'uuid': USER}
            if protocol == 'vmess': user['alterId'] = 0
            server.update(users=[user], tls=tls_server)
        elif protocol == 'trojan':
            tls_server['alpn'] = ['http/1.1']
            server.update(users=[{'password': PASSWORD}], tls=tls_server)
        elif protocol == 'tuic':
            server.update(users=[{'uuid': USER, 'password': PASSWORD}], tls=tls_server, congestion_control='bbr')
            client.update(uuid=USER, password=PASSWORD, tls=tls_client, congestion_control='bbr')
        elif protocol == 'shadowsocks':
            method, password = ('aes-128-gcm', PASSWORD) if case.endswith('-aead') else ('2022-blake3-aes-128-gcm', base64.b64encode(bytes(range(16))).decode())
            server.update(method=method, password=password)
            client.update(method=method, password=password)
        return 'sing_box', {'log': {'level': 'warn'}, 'inbounds': [server], 'outbounds': [{'type': 'direct'}]}, client
    if case == 'xray-direct':
        return 'xray', None, {'protocol': 'freedom'}
    protocol, transport = case.split('-', 1)
    if transport.startswith('reality-'):
        transport = transport[len('reality-'):]
    protocol = {'ss': 'shadowsocks'}.get(protocol, protocol)
    server = {'listen': '127.0.0.1', 'port': port, 'protocol': protocol}
    client = {'protocol': protocol}
    if protocol in ('vless', 'vmess'):
        user = {'id': USER}
        if protocol == 'vless':
            user['encryption'] = 'none'
        else:
            user['security'] = 'auto'
        server['settings'] = {'clients': [{'id': USER}], **({'decryption': 'none'} if protocol == 'vless' else {})}
        client['settings'] = {'vnext': [{'address': '127.0.0.1', 'port': port, 'users': [user]}]}
    elif protocol == 'trojan':
        server['settings'] = {'clients': [{'password': PASSWORD}]}
        client['settings'] = {'servers': [{'address': '127.0.0.1', 'port': port, 'password': PASSWORD}]}
    elif protocol == 'shadowsocks':
        server['settings'] = {'method': 'aes-128-gcm', 'password': PASSWORD, 'network': 'tcp,udp'}
        client['settings'] = {'servers': [{'address': '127.0.0.1', 'port': port, 'method': 'aes-128-gcm', 'password': PASSWORD}]}
    if transport != 'plain':
        network = 'raw' if transport in ('tls','reality','vision') else transport.split(':')[0]
        server_stream = {'network': network, 'security': 'tls', 'tlsSettings': {'certificates': [{'certificateFile': str(cert), 'keyFile': str(key)}]}}
        client_stream = {'network': network, 'security': 'tls', 'tlsSettings': {'serverName': 'localhost', 'certificates': [{'certificateFile': str(cert), 'usage': 'verify'}]}}
        if network == 'xhttp':
            settings = {'path': '/astra-test', 'mode': transport.split(':')[1]}
            server_stream['xhttpSettings'] = settings
            client_stream['xhttpSettings'] = settings
        elif network in ('ws', 'httpupgrade'):
            server_stream[network + 'Settings'] = {'path': '/astra-test'}
            client_stream[network + 'Settings'] = {'path': '/astra-test'}
        elif network == 'grpc':
            server_stream['grpcSettings'] = {'serviceName': 'astra-test'}
            client_stream['grpcSettings'] = {'serviceName': 'astra-test'}
        server['streamSettings'], client['streamSettings'] = server_stream, client_stream
    return 'xray', {'log': {'loglevel': 'warning'}, 'inbounds': [server], 'outbounds': [{'protocol': 'freedom'}]}, client

CASES = ['socks-plain', 'socks-auth', 'http-plain', 'http-auth', 'ss-plain', 'sing-shadowsocks', 'sing-vmess', 'vmess-tls', 'trojan-tls', 'vless-tls',
         'vless-ws', 'vless-grpc', 'vless-httpupgrade', 'vless-xhttp:packet-up', 'vless-xhttp:stream-up',
         'vless-xhttp:stream-one', 'vless-reality', 'vless-reality-grpc', 'vless-reality-xhttp:stream-one', 'vless-reality-xhttp:stream-up', 'vless-reality-xhttp:packet-up', 'vless-vision', 'sing-hysteria2', 'sing-tuic']

def run_case(case, binaries, engines, echo_port, payload_bytes=65536, trailer=False, udp=False, udp_edge=False, bad_auth=False, tuic_udp_mode="native", tls_payload=False, udp_sizes=None, bad_reality=False, fault=None, hy2_obfs=False):
    with tempfile.TemporaryDirectory(prefix='astra-matrix-') as temp:
        d = pathlib.Path(temp)
        cert, key = tls_material(d)
        peer_port, proxy_port = free_port(), free_port()
        engine, server_config, node = fixture(case, peer_port, cert, key)
        if hy2_obfs:
            server_config['inbounds'][0]['obfs'] = {'type': 'salamander', 'password': PASSWORD}
        reality = case.startswith('vless-reality') or case == 'vless-vision'
        if reality:
            keys=subprocess.check_output([str(engines/'xray'),'x25519'],text=True)
            private=next(line.split(': ',1)[1] for line in keys.splitlines() if line.startswith('PrivateKey:'))
            public=next(line.split(': ',1)[1] for line in keys.splitlines() if 'PublicKey' in line)
            if bad_reality:
                other = subprocess.check_output([str(engines/'xray'), 'x25519'], text=True)
                public = next(line.split(': ', 1)[1] for line in other.splitlines() if 'PublicKey' in line)
            cover_port=free_port()
            inbound=server_config['inbounds'][0]
            inbound['streamSettings'].pop('tlsSettings', None)
            inbound['streamSettings'].update(security='reality', realitySettings={'target':f'127.0.0.1:{cover_port}','serverNames':['localhost'],'privateKey':private,'shortIds':['0123456789abcdef']})
            if case=='vless-vision':inbound['settings']['clients'][0]['flow']='xtls-rprx-vision'
        proxy = d / 'proxy.toml' 
        rule = d / 'rule.toml'
        rule.write_text(f'socket="{d}/rules.sock"\ndefault_action="proxy:edge"\n')
        if case in ('xray-direct','sing-direct'):
            raise ValueError('reference-engine client tests removed: Astra must be native Rust')
        if case.startswith('sing-'):
            proto=case[5:].removesuffix('-aead')
            transport='tcp'
        else:
            proto,wire=case.split('-',1)
            if wire.startswith('reality-'): wire=wire[len('reality-'):]
            proto={'ss':'shadowsocks'}.get(proto,proto)
            transport='tcp' if wire in ('plain','tls','reality','vision') else wire.split(':')[0]
        cfg=f'protocol="{proto}"\nserver="127.0.0.1"\nport={peer_port}\n'
        if proto == 'tuic': cfg += f'tuic_udp_mode="{tuic_udp_mode}"\n'
        if case=='vless-vision':cfg+='flow="xtls-rprx-vision"\n'
        if proto in ('vless','vmess','tuic'): cfg+=f'uuid="{USER}"\n'
        if proto in ('trojan','hysteria2','tuic'): cfg+=f'password="{PASSWORD}"\n'
        if proto=='shadowsocks':
            cipher='2022-blake3-aes-128-gcm' if case.startswith('sing-') and not case.endswith('-aead') else 'aes-128-gcm'
            password=base64.b64encode(bytes(range(16))).decode() if cipher.startswith('2022-') else PASSWORD
            cfg+=f'cipher="{cipher}"\npassword="{password}"\n'
        if proto=='hysteria2':
            if hy2_obfs: cfg+=f'obfs_password="{PASSWORD}"\n'
            import hashlib
            der=ssl.PEM_cert_to_DER_cert(cert.read_text())
            cfg+=f'pin_sha256="{hashlib.sha256(der).hexdigest()}"\n'
        if transport!='tcp':
            kind={'httpupgrade':'http_upgrade'}.get(transport,transport)
            cfg+=f'[outbounds.config.transport]\nkind="{kind}"\npath="/astra-test"\nservice_name="astra-test"\n'
            if transport=='xhttp':cfg+=f'mode="{case.split(":")[1]}"\n'
        if proto not in ('shadowsocks',) and not case.endswith('-plain'):
            cfg+='[outbounds.config.tls]\nserver_name="localhost"\n'
            if reality:cfg+=f'reality_public_key="{public}"\nreality_short_id="0123456789abcdef"\nfingerprint="chrome"\n'
            elif proto!='hysteria2':cfg+=f'ca_file="{d / "ca.pem"}"\n'
        if fault == 'credential':
            if proto in ('vless', 'vmess'):
                cfg = cfg.replace(USER, '00000000-0000-0000-0000-000000000002')
            elif proto == 'shadowsocks':
                wrong = base64.b64encode(bytes([99])*16).decode() if cipher.startswith('2022-') else 'wrong-password'
                cfg = cfg.replace(f'password="{password}"', f'password="{wrong}"')
            else:
                cfg = cfg.replace(PASSWORD, 'incorrect-password')
        elif fault == 'server-name':
            cfg = cfg.replace('server_name="localhost"', 'server_name="wrong.example"')
        elif fault == 'untrusted-ca':
            cfg = cfg.replace(f'ca_file="{d / "ca.pem"}"\n', '')
        elif fault == 'pin':
            cfg = cfg.replace(hashlib.sha256(der).hexdigest(), '00'*32)
        proxy.write_text(f'rule_socket="{d}/rules.sock"\nsocks5_listen="127.0.0.1:{proxy_port}"\nconnect_timeout_secs=10\nshutdown_timeout_secs=1\n[[outbounds]]\nkind="native"\ntag="edge"\n[outbounds.config]\n'+cfg)
        if case.startswith(('socks-', 'http-', 'sing-socks-')):
            kind = 'socks5' if case.startswith(('socks-', 'sing-socks-')) else 'http'
            credentials = f'username="astra"\npassword="{PASSWORD}"\n' if case.endswith('-auth') else ''
            if bad_auth:
                credentials = credentials.replace(PASSWORD, 'incorrect-password')
            proxy.write_text(f'rule_socket="{d}/rules.sock"\nsocks5_listen="127.0.0.1:{proxy_port}"\nshutdown_timeout_secs=1\n[[outbounds]]\nkind="{kind}"\ntag="edge"\nserver="127.0.0.1:{peer_port}"\n'+credentials)
        with contextlib.ExitStack() as stack:
            if reality:
                cover,cover_log=stack.enter_context(process(['openssl','s_server','-accept',f'127.0.0.1:{cover_port}','-cert',str(cert),'-key',str(key),'-www','-alpn','h2'],d,'cover'))
                wait_port(cover_port,cover,cover_log)
            if server_config:
                config_path = write_json(d / 'server.json', server_config)
                binary = engines / ('xray' if engine == 'xray' else 'sing-box')
                command = [str(binary), 'run', '-config' if engine == 'xray' else '-c', str(config_path)]
                peer, peer_log = stack.enter_context(process(command, d, 'server'))
                if case in ('sing-hysteria2', 'sing-tuic'):
                    time.sleep(.3)
                    if peer.poll() is not None:
                        raise RuntimeError(peer_log.read_text())
                else:
                    wait_port(peer_port, peer, peer_log)
            rules, rules_log = stack.enter_context(process([str(binaries / 'astra-rule'), str(rule)], d, 'rule'))
            for _ in range(100):
                if (d / 'rules.sock').exists():
                    break
                if rules.poll() is not None:
                    raise RuntimeError(rules_log.read_text())
                time.sleep(.02)
            app, app_log = stack.enter_context(process([str(binaries / 'astra-proxy'), str(proxy)], d, 'proxy'))
            wait_port(proxy_port, app, app_log)
            if fault:
                try:
                    with socket.create_connection(('127.0.0.1', proxy_port), timeout=15) as denied:
                        denied.sendall(b'\x05\x01\x00')
                        assert exact(denied, 2) == b'\x05\x00'
                        denied.sendall(b'\x05\x01\x00\x01\x7f\x00\x00\x01' + echo_port.to_bytes(2, 'big'))
                        status = exact(denied, 10)[1]
                        silent = False
                        if status == 0:
                            try:
                                denied.sendall(b'unauthorized-echo-must-never-arrive')
                                data = denied.recv(65536)
                            except (ConnectionResetError, BrokenPipeError):
                                data = b''
                            except (socket.timeout, TimeoutError):
                                if case != 'ss-plain' or fault != 'credential':
                                    raise
                                silent = True
                                data = b''
                            assert data == b'', 'invalid node delivered application data'
                    print('PASS', case, fault, 'no application data during 15-second probe (silent peer)' if silent else 'rejected without application data', flush=True)
                except Exception as error:
                    raise RuntimeError(str(error) + "\n" + app_log.read_text()[-3000:]) from error
                return
            if tls_payload:
                tls_server = stack.enter_context(EchoServer(('127.0.0.1', 0), TlsEcho))
                tls_server.errors = []
                tls_server.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
                tls_server.tls.minimum_version = ssl.TLSVersion.TLSv1_3
                tls_server.tls.load_cert_chain(str(cert), str(key))
                threading.Thread(target=tls_server.serve_forever, daemon=True).start()
                stack.callback(tls_server.shutdown)
                try:
                    tls_probe(proxy_port, tls_server.server_address[1], payload_bytes, d / 'ca.pem')
                    assert not tls_server.errors, tls_server.errors
                    if case == 'vless-vision':
                        log = app_log.read_text()
                        assert 'XTLS Vision direct write passthrough enabled' in log, 'Vision did not switch writes to raw'
                        assert 'XTLS Vision direct passthrough enabled' in log, 'Vision did not switch reads to raw'
                except Exception as error:
                    raise RuntimeError(str(error) + "\n" + app_log.read_text()[-5000:] + "\n" + peer_log.read_text()[-3000:]) from error
                print('PASS', case, f'{payload_bytes} bytes over verified inner TLS 1.3' + (' + raw read/write switch' if case == 'vless-vision' else ''), flush=True)
                return
            if bad_auth or bad_reality:
                with socket.create_connection(('127.0.0.1', proxy_port), timeout=15) as denied:
                    denied.sendall(b'\x05\x01\x00')
                    assert exact(denied, 2) == b'\x05\x00'
                    denied.sendall(b'\x05\x01\x00\x01\x7f\x00\x00\x01' + echo_port.to_bytes(2, 'big'))
                    assert exact(denied, 10)[1] == 5, 'bad credentials were accepted'
                print('PASS', case, 'wrong REALITY public key rejected' if bad_reality else 'invalid upstream credentials rejected', flush=True)
                return
            if udp:
                try:
                    udp_probe(proxy_port, echo_port, udp_edge, udp_sizes)
                except Exception as error:
                    raise RuntimeError(str(error) + "\n" + app_log.read_text()[-5000:] + "\n" + peer_log.read_text()[-3000:]) from error
                print("PASS", case, "UDP IPv4/IPv6/domain, " + (str(udp_sizes) + " bytes" if udp_sizes is not None else ("0..8192 bytes" if udp_edge else "1..4096 bytes")), flush=True)
                return
            try:
                with socket.create_connection(('127.0.0.1', proxy_port), timeout=15) as sock:
                    sock.sendall(b'\x05\x01\x00')
                    assert exact(sock, 2) == b'\x05\x00'
                    # Domain-form target verifies end-to-end destination forwarding.
                    host = b'localhost'
                    sock.sendall(b'\x05\x01\x00\x03' + bytes([len(host)]) + host + echo_port.to_bytes(2, 'big'))
                    response = exact(sock, 10)
                    assert response[1] == 0, response
                    payload = (bytes(range(256)) * ((payload_bytes + 255) // 256))[:payload_bytes]
                    send_errors = []
                    def upload():
                        try:
                            sock.sendall(payload)
                            if trailer:
                                sock.shutdown(socket.SHUT_WR)
                        except Exception as error:
                            send_errors.append(error)
                    sender = threading.Thread(target=upload, daemon=True)
                    sender.start()
                    assert exact(sock, len(payload)) == payload
                    if trailer:
                        assert exact(sock, len(TRAILER)) == TRAILER
                    sender.join(timeout=15)
                    assert not sender.is_alive(), 'upload did not finish'
                    if send_errors:
                        raise send_errors[0]
                    if not trailer:
                        sock.shutdown(socket.SHUT_WR)
                    assert sock.recv(1) == b'', 'half-close did not complete'
            except Exception as error:
                raise RuntimeError(str(error) + '\n' + app_log.read_text()[-5000:] + ('\n' + peer_log.read_text()[-3000:] if server_config else '')) from error
    print('PASS', case, f'{payload_bytes} bytes' + (' + early upload EOF + response trailer' if trailer else ' + close after echo'), flush=True)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT / 'target/debug')
    parser.add_argument('--engine-dir', type=pathlib.Path, default=ROOT / '.test-tools/bin')
    parser.add_argument('--case', action='append', choices=CASES + ['vless-plain', 'vmess-plain', 'sing-vless', 'sing-shadowsocks-aead', 'sing-trojan', 'sing-socks-plain', 'sing-socks-auth'])
    parser.add_argument('--payload-bytes', type=int, default=65536)
    parser.add_argument('--tuic-udp-mode', choices=['native', 'quic'], default='native')
    parser.add_argument('--hy2-obfs', action='store_true', help='test Hysteria2 Salamander obfuscation')
    parser.add_argument('--fault', choices=['credential', 'server-name', 'untrusted-ca', 'pin'])
    parser.add_argument('--bad-reality', action='store_true', help='verify rejection of a different valid REALITY public key')
    parser.add_argument('--bad-auth', action='store_true', help='verify upstream authentication rejection for socks-auth/http-auth')
    parser.add_argument('--udp-edge', action='store_true', help='also test empty and 8192-byte datagrams')
    parser.add_argument('--udp-size', action='append', type=int, help='test specified UDP payload sizes; repeat for multiple sizes')
    parser.add_argument('--udp', action='store_true')
    parser.add_argument('--tls-payload', action='store_true', help='verified inner TLS 1.3 echo; checks Vision raw switch (requires RUST_LOG=debug)')
    parser.add_argument('--trailer', action='store_true', help='require data sent by destination after client half-close')
    args = parser.parse_args()
    args.udp = args.udp or bool(args.udp_size)
    if args.udp_size is not None and any(n < 0 or n > 65000 for n in args.udp_size):
        parser.error("udp-size must be between 0 and 65000")
    if args.hy2_obfs and (not args.case or any(c != 'sing-hysteria2' for c in args.case)):
        parser.error('hy2-obfs requires explicit sing-hysteria2 cases')
    if args.fault:
        if not args.case or args.udp or args.udp_edge or args.bad_auth or args.bad_reality or args.tls_payload or args.trailer:
            parser.error('fault requires explicit native TCP cases without other diagnostic flags')
        allowed = {
            'credential': {'vless-tls','vmess-tls','trojan-tls','ss-plain','sing-shadowsocks','sing-hysteria2','sing-tuic'},
            'server-name': {'vless-tls','vmess-tls','trojan-tls','sing-tuic'},
            'untrusted-ca': {'vless-tls','vmess-tls','trojan-tls','sing-tuic'},
            'pin': {'sing-hysteria2'},
        }
        if any(c not in allowed[args.fault] for c in args.case):
            parser.error('fault is not applicable to one of the selected cases')
    if args.bad_reality and (args.udp or args.udp_edge or args.trailer or args.tls_payload or args.bad_auth or not args.case or any(not c.startswith('vless-reality') and c != 'vless-vision' for c in args.case)):
        parser.error('bad-reality requires REALITY cases without UDP, TLS-payload, trailer or bad-auth flags')
    if args.bad_auth and (args.udp or args.udp_edge or not args.case or any(case not in ("socks-auth", "http-auth") for case in args.case)):
        parser.error("bad-auth requires socks-auth/http-auth cases without UDP flags")
    if args.tls_payload and (args.udp or args.udp_edge or args.trailer or args.bad_auth):
        parser.error('tls-payload cannot be combined with UDP, trailer or bad-auth')
    if args.tls_payload and not ssl.HAS_TLSv1_3:
        parser.error('tls-payload requires Python linked with a TLS 1.3 capable OpenSSL')
    if args.tls_payload:
        os.environ['RUST_LOG'] = 'debug'
    args.udp = args.udp or args.udp_edge
    if not 1 <= args.payload_bytes <= 64 * 1024 * 1024:
        parser.error('payload-bytes must be between 1 and 67108864')
    with (UdpEchoServer(('127.0.0.1', 0), UdpEcho) if args.udp else EchoServer(('127.0.0.1', 0), Echo)) as echo:
        echo.send_trailer = args.trailer
        echo6 = UdpEchoServer6(('::1', echo.server_address[1]), UdpEcho) if args.udp else None
        if echo6:
            threading.Thread(target=echo6.serve_forever, daemon=True).start()
        threading.Thread(target=echo.serve_forever, daemon=True).start()
        failed = []
        for case in args.case or CASES:
            try:
                run_case(case, args.bin_dir.resolve(), args.engine_dir.resolve(), echo.server_address[1], args.payload_bytes, args.trailer, args.udp, args.udp_edge, args.bad_auth, args.tuic_udp_mode, args.tls_payload, args.udp_size, args.bad_reality, args.fault, args.hy2_obfs)
            except Exception as error:
                failed.append(case)
                print('FAIL', case, str(error), flush=True)
        echo.shutdown()
        if echo6:
            echo6.shutdown()
            echo6.server_close()
    if failed:
        raise SystemExit('Failed cases: ' + ', '.join(failed))

if __name__ == '__main__':
    main()
