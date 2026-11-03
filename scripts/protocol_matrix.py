#!/usr/bin/env python3
"""Local payload interoperability tests using real protocol servers, without public nodes."""
from astra_tools.processes import process, wait_port
from astra_tools.workspace import resolve_binary_dir, reference_engine, reference_engine_dir
import argparse
from astra_tools.fixtures.protocols import USER, PASSWORD, write_json, tls_material, fixture, openssl_binary
from astra_tools.net import TRAILER, Echo, TlsEcho, tls_probe, UdpEcho, UdpEchoServer, UdpEchoServer6, udp_probe, EchoServer, free_port, exact
import base64
import contextlib
import json
import os
import pathlib
import random
import signal
import socket
import socketserver
import ssl
import subprocess
import tempfile
import threading
import time
from astra_tools.platform import process_flags, interrupt

ROOT = pathlib.Path(__file__).resolve().parents[1]
















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
            keys=subprocess.check_output([str(reference_engine('xray', engines)),'x25519'],text=True)
            private=next(line.split(': ',1)[1] for line in keys.splitlines() if line.startswith('PrivateKey:'))
            public=next(line.split(': ',1)[1] for line in keys.splitlines() if 'PublicKey' in line)
            if bad_reality:
                other = subprocess.check_output([str(reference_engine('xray', engines)), 'x25519'], text=True)
                public = next(line.split(': ', 1)[1] for line in other.splitlines() if 'PublicKey' in line)
            cover_port=free_port()
            inbound=server_config['inbounds'][0]
            inbound['streamSettings'].pop('tlsSettings', None)
            inbound['streamSettings'].update(security='reality', realitySettings={'target':f'127.0.0.1:{cover_port}','serverNames':['localhost'],'privateKey':private,'shortIds':['0123456789abcdef']})
            if case=='vless-vision':inbound['settings']['clients'][0]['flow']='xtls-rprx-vision'
        proxy = d / 'proxy.toml' 
        rule = d / 'rule.toml'
        rule.write_text(f'socket="{d.as_posix()}/rules.sock"\ndirect_local=false\ndefault_action="proxy:edge"\n')
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
            elif proto!='hysteria2':cfg+=f'ca_file="{(d / "ca.pem").as_posix()}"\n'
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
            cfg = cfg.replace(f'ca_file="{(d / "ca.pem").as_posix()}"\n', '')
        elif fault == 'pin':
            cfg = cfg.replace(hashlib.sha256(der).hexdigest(), '00'*32)
        proxy.write_text(f'rule_socket="{d.as_posix()}/rules.sock"\nsocks5_listen="127.0.0.1:{proxy_port}"\nconnect_timeout_secs=10\nshutdown_timeout_secs=1\n[[outbounds]]\nkind="native"\ntag="edge"\n[outbounds.config]\n'+cfg)
        if case.startswith(('socks-', 'http-', 'sing-socks-')):
            kind = 'socks5' if case.startswith(('socks-', 'sing-socks-')) else 'http'
            credentials = f'username="astra"\npassword="{PASSWORD}"\n' if case.endswith('-auth') else ''
            if bad_auth:
                credentials = credentials.replace(PASSWORD, 'incorrect-password')
            proxy.write_text(f'rule_socket="{d.as_posix()}/rules.sock"\nsocks5_listen="127.0.0.1:{proxy_port}"\nshutdown_timeout_secs=1\n[[outbounds]]\nkind="{kind}"\ntag="edge"\nserver="127.0.0.1:{peer_port}"\n'+credentials)
        with contextlib.ExitStack() as stack:
            if reality:
                cover,cover_log=stack.enter_context(process([openssl_binary(),'s_server','-accept',f'127.0.0.1:{cover_port}','-cert',str(cert),'-key',str(key),'-www','-alpn','h2'],d,'cover'))
                wait_port(cover_port,cover,cover_log)
            if server_config:
                config_path = write_json(d / 'server.json', server_config)
                binary = reference_engine('xray' if engine == 'xray' else 'sing-box', engines)
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
            phase = 'connect'
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
                    phase = 'payload echo'
                    assert exact(sock, len(payload)) == payload
                    if trailer:
                        phase = 'response trailer after upload EOF'
                        assert exact(sock, len(TRAILER)) == TRAILER
                    sender.join(timeout=15)
                    assert not sender.is_alive(), 'upload did not finish'
                    if send_errors:
                        raise send_errors[0]
                    if not trailer:
                        sock.shutdown(socket.SHUT_WR)
                    phase = 'read EOF after complete echo'
                    assert sock.recv(1) == b'', 'half-close did not complete'
            except Exception as error:
                raise RuntimeError(phase + ': ' + str(error) + '\n' + app_log.read_text()[-5000:] + ('\n' + peer_log.read_text()[-3000:] if server_config else '')) from error
    print('PASS', case, f'{payload_bytes} bytes' + (' + early upload EOF + response trailer' if trailer else ' + close after echo'), flush=True)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=None)
    parser.add_argument('--engine-dir', '--engines-dir', dest='engine_dir', type=pathlib.Path, default=None)
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
    args = parser.parse_args(); args.bin_dir = resolve_binary_dir(args.bin_dir, profile='debug')
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
                run_case(case, args.bin_dir.resolve(), reference_engine_dir(args.engine_dir), echo.server_address[1], args.payload_bytes, args.trailer, args.udp, args.udp_edge, args.bad_auth, args.tuic_udp_mode, args.tls_payload, args.udp_size, args.bad_reality, args.fault, args.hy2_obfs)
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
