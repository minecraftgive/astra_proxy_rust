"""Synthetic local credentials and reference-server fixtures."""
import base64
import json
import os
from pathlib import Path
import shutil
import subprocess

USER = '00000000-0000-0000-0000-000000000001'

PASSWORD = 'astra-local-test-password'

def write_json(path, data):
    path.write_text(json.dumps(data, indent=2))
    return path

def openssl_binary():
    openssl = shutil.which('openssl')
    if not openssl and os.name == 'nt':
        git = Path(os.environ.get('ProgramFiles', 'C:/Program Files')) / 'Git'
        openssl = next((str(path) for path in (git/'mingw64/bin/openssl.exe', git/'usr/bin/openssl.exe') if path.is_file()), None)
    if not openssl:
        raise RuntimeError('OpenSSL is required to generate a temporary test CA')
    return openssl

def tls_material(directory):
    openssl = openssl_binary()
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
        command[0] = openssl
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
