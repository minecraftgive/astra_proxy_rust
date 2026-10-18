#!/usr/bin/env python3
"""VLESS Encryption + Vision + pinned TLS + XHTTP auto against a local Xray peer."""
import argparse
import contextlib
import hashlib
import pathlib
import socket
import ssl
import subprocess
import tempfile
import threading
from protocol_matrix import ROOT, USER, Echo, EchoServer, exact, free_port, process, tls_material, tls_probe, TlsEcho, wait_port, write_json
from core_regression import socks, wait_socket


def check(binaries, wrong_pin=False, inner_tls=False):
    with tempfile.TemporaryDirectory(prefix='astra-vless-encryption-') as td, contextlib.ExitStack() as stack:
        d = pathlib.Path(td)
        cert, key = tls_material(d)
        keys = subprocess.check_output([str(ROOT/'.test-tools/bin/xray'), 'x25519'], text=True)
        private = next(line.split(': ',1)[1] for line in keys.splitlines() if line.startswith('PrivateKey:'))
        public = next(line.split(': ',1)[1] for line in keys.splitlines() if 'PublicKey' in line)
        peer_port, proxy_port = free_port(), free_port()
        server = {'log':{'loglevel':'debug'}, 'inbounds':[{'listen':'127.0.0.1','port':peer_port,'protocol':'vless',
            'settings':{'clients':[{'id':USER,'flow':'xtls-rprx-vision'}], 'decryption':f'mlkem768x25519plus.native.600s.{private}'},
            'streamSettings':{'network':'xhttp','security':'tls','tlsSettings':{'certificates':[{'certificateFile':str(cert),'keyFile':str(key)}]},'xhttpSettings':{'path':'/astra-test','mode':'auto'}}}],
            'outbounds':[{'protocol':'freedom'}]}
        path = write_json(d/'server.json',server)
        peer, peer_log = stack.enter_context(process([str(ROOT/'.test-tools/bin/xray'),'run','-config',str(path)],d,'peer'))
        wait_port(peer_port,peer,peer_log)
        echo = stack.enter_context(EchoServer(('127.0.0.1',0),TlsEcho if inner_tls else Echo))
        if inner_tls:
            echo.errors = []
            echo.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            echo.tls.minimum_version = ssl.TLSVersion.TLSv1_3
            echo.tls.load_cert_chain(str(cert), str(key))
        threading.Thread(target=echo.serve_forever,daemon=True).start()
        stack.callback(echo.shutdown)
        pin = hashlib.sha256(ssl.PEM_cert_to_DER_cert(cert.read_text())).hexdigest()
        if wrong_pin: pin = '00'*32
        rules, proxy, uds = d/'rules.toml',d/'proxy.toml',d/'rules.sock'
        rules.write_text(f'socket="{uds}"\ndefault_action="proxy:edge"\n')
        proxy.write_text(f'''rule_socket="{uds}"
socks5_listen="127.0.0.1:{proxy_port}"
shutdown_timeout_secs=1
connect_timeout_secs=10
[[outbounds]]
kind="native"
tag="edge"
[outbounds.config]
protocol="vless"
server="127.0.0.1"
port={peer_port}
uuid="{USER}"
flow="xtls-rprx-vision"
encryption="mlkem768x25519plus.native.0rtt.{public}"
[outbounds.config.tls]
server_name="localhost"
fingerprint="chrome"
cert_pin_sha256="{pin}"
[outbounds.config.transport]
kind="xhttp"
path="/astra-test"
mode="auto"
''')
        rule, rule_log=stack.enter_context(process([str(binaries/'astra-rule'),str(rules)],d,'rules'))
        wait_socket(uds,rule,rule_log)
        app, app_log=stack.enter_context(process([str(binaries/'astra-proxy'),str(proxy)],d,'proxy'))
        wait_port(proxy_port,app,app_log)
        try:
            # Multiple dials exercise initial 1-RTT and cached 0-RTT tickets.
            for attempt in range(3):
                if inner_tls:
                    tls_probe(proxy_port, echo.server_address[1], 1024*1024, d/'ca.pem')
                    continue
                client,status=socks(proxy_port,echo.server_address[1])
                with client:
                    client.settimeout(15)
                    if wrong_pin:
                        assert status != 0, 'incorrect certificate pin accepted'
                        break
                    assert status == 0,status
                    payload=bytes(range(256))*256
                    sender=threading.Thread(target=client.sendall,args=(payload,),daemon=True)
                    sender.start()
                    assert exact(client,len(payload))==payload
                    sender.join(timeout=5)
                    assert not sender.is_alive()
        except Exception as error:
            raise RuntimeError(f'{error}\n{app_log.read_text()[-5000:]}\n{peer_log.read_text()[-6000:]}') from error
        if inner_tls:
            logs = app_log.read_text()
            assert 'XTLS Vision direct write passthrough enabled' in logs
            assert 'XTLS Vision direct passthrough enabled' in logs
        print('PASS encrypted Vision TLS 1.3 raw switch: 3 x 1 MiB' if inner_tls else 'PASS certificate pin mismatch rejected' if wrong_pin else 'PASS VLESS Encryption + Vision + pinned Chrome TLS + XHTTP auto: 3 x 64 KiB (ticket reuse)',flush=True)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--inner-tls', action='store_true')
    parser.add_argument('--bin-dir',type=pathlib.Path,default=ROOT/'target/debug')
    args=parser.parse_args()
    import os
    os.environ['RUST_LOG'] = 'astra_proxy=debug,meow_proxy=debug'
    if args.inner_tls and not ssl.HAS_TLSv1_3:
        parser.error('inner-tls requires a TLS 1.3-capable Python runtime')
    check(args.bin_dir.resolve())
    check(args.bin_dir.resolve(),wrong_pin=True)
    if args.inner_tls:
        check(args.bin_dir.resolve(),inner_tls=True)


if __name__=='__main__':main()
