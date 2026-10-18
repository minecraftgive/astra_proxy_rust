#!/usr/bin/env python3
"""Local native server/client interoperability; never enables TUN or system proxy."""
import argparse
import base64
import contextlib
import hashlib
import pathlib
import socket
import ssl
import subprocess
import tempfile
import threading
import time
from platform_support import executable
from protocol_matrix import Echo, UdpEchoServer, UdpEchoServer6, UdpEcho, free_port, exact, process, wait_port, udp_probe
import socketserver

ROOT = pathlib.Path(__file__).resolve().parents[1]
USER = '52ded580-44fd-4674-8fe3-957b28d66143'
PASSWORD = 'astra-native-server-test'
CASES = ['socks5', 'http', 'ss-aes128', 'ss-aes256', 'ss-chacha', 'ss2022-aes128', 'ss2022-aes256', 'ss2022-chacha', 'vmess', 'vmess-chacha', 'vless', 'trojan', 'hysteria2', 'tuic', 'tuic-quic', 'vless-ws', 'vless-http_upgrade', 'vless-grpc', 'vless-xhttp']

def command(binary, control, action):
    return subprocess.run([str(binary), '--control', str(control), action], capture_output=True, text=True, timeout=25)

def connect(proxy_port, echo_port):
    stream=socket.create_connection(('127.0.0.1',proxy_port),timeout=15)
    stream.sendall(b'\x05\x01\x00')
    assert exact(stream,2)==b'\x05\x00'
    stream.sendall(b'\x05\x01\x00\x03\x09localhost'+echo_port.to_bytes(2,'big'))
    response=exact(stream,10)
    return stream,response[1]

def run(case,bins,echo_port,udp_port,bad=False,private=False,hosts=None):
    with tempfile.TemporaryDirectory(prefix='astra-native-server-') as temp, contextlib.ExitStack() as stack:
        d=pathlib.Path(temp);server=bins/executable('astra-server-core');proxy=bins/executable('astra-proxy');control=d/'control.sock'
        subprocess.run([str(server),'--generate-cert',str(d/'tls'),'localhost'],check=True,stdout=subprocess.DEVNULL)
        cert=d/'tls/cert.pem';key=d/'tls/key.pem';peer_port,proxy_port=free_port(),free_port()
        proto=case.split('-')[0];proto={'ss':'shadowsocks','ss2022':'shadowsocks'}.get(proto,proto)
        password=PASSWORD;cipher=None
        if case.startswith(('ss-','ss2022-')):
            cipher={'aes128':'aes-128-gcm','aes256':'aes-256-gcm','chacha':'chacha20-ietf-poly1305'}[case.split('-')[1]]
            if case.startswith('ss2022-'):
                cipher={'aes128':'2022-blake3-aes-128-gcm','aes256':'2022-blake3-aes-256-gcm','chacha':'2022-blake3-chacha20-poly1305'}[case.split('-')[1]]
                password=base64.b64encode(bytes(range(16 if '128' in cipher else 32))).decode()
        transport=case.split('-')[1] if case.startswith('vless-') else 'tcp'
        cfg=f'allow_private_targets={str(not private).lower()}\nshutdown_timeout_secs=0\n[[listeners]]\ntag="test"\nprotocol="{proto}"\nlisten="127.0.0.1:{peer_port}"\n'
        if transport!='tcp':cfg+=f'transport="{transport}"\npath="/astra-test"\nservice_name="astra-test"\n'
        if cipher:cfg+=f'cipher="{cipher}"\npassword="{password}"\n'
        else:
            if proto not in ('socks5','http'):cfg+=f'[listeners.tls]\ncertificate="{cert.as_posix()}"\nprivate_key="{key.as_posix()}"\n'
            cfg+=f'[[listeners.users]]\nname="astra"\nuuid="{USER}"\npassword="{password}"\n'
        (d/'server.toml').write_text(cfg)
        p,server_log=stack.enter_context(process([str(server),'--config',str(d/'server.toml'),'--control',str(control)],d,'server'))
        for _ in range(100):
            if control.exists() and command(server,control,'--status').returncode==0:break
            if p.poll() is not None:raise RuntimeError(server_log.read_text())
            time.sleep(.05)
        client=f'standalone=true\nstandalone_outbound="server"\nsocks5_listen="127.0.0.1:{proxy_port}"\nshutdown_timeout_secs=0\n[[outbounds]]\ntag="server"\n'
        client_password=(base64.b64encode(bytes([99])*(16 if '128' in cipher else 32)).decode() if cipher and cipher.startswith('2022-') else 'incorrect-password') if bad else password
        client_id='52ded580-44fd-4674-8fe3-957b28d66144' if bad else USER
        if proto in ('socks5','http'):
            client+=f'kind="{proto}"\nserver="127.0.0.1:{peer_port}"\nusername="astra"\npassword="{client_password}"\n'
        else:
            client+=f'kind="native"\n[outbounds.config]\nprotocol="{proto}"\nserver="127.0.0.1"\nport={peer_port}\n'
            if proto in ('vless','vmess','tuic'):client+=f'uuid="{client_id}"\n'
            if proto in ('shadowsocks','trojan','hysteria2','tuic'):client+=f'password="{client_password}"\n'
            if cipher:client+=f'cipher="{cipher}"\n'
            if case.startswith('vmess-'):client+=f'cipher="{dict(chacha="chacha20-poly1305",none="none")[case.split("-")[1]]}"\n'
            if case=='tuic-quic':client+='tuic_udp_mode="quic"\n'
            if proto=='hysteria2':client+=f'pin_sha256="{hashlib.sha256(ssl.PEM_cert_to_DER_cert(cert.read_text())).hexdigest()}"\n'
            if transport!='tcp':client+=f'[outbounds.config.transport]\nkind="{transport}"\npath="/astra-test"\nservice_name="astra-test"\n'+('mode="stream-one"\n' if transport=='xhttp' else '')
            if proto!='shadowsocks':
                client+='[outbounds.config.tls]\nserver_name="localhost"\n'
                if proto!='hysteria2':client+=f'ca_file="{cert.as_posix()}"\n'
        (d/'proxy.toml').write_text(client)
        app,app_log=stack.enter_context(process([str(proxy),str(d/'proxy.toml')],d,'proxy'));wait_port(proxy_port,app,app_log)
        try:
            stream,status=connect(proxy_port,echo_port)
            with stream:
                if bad or private:
                    if status==0:
                        stream.sendall(b'must-not-echo')
                        try:data=stream.recv(13)
                        except OSError:data=b''
                        assert data!=b'must-not-echo','unauthorized traffic relayed'
                else:
                    assert status==0,status
                    for n in (1,4096,65536,1048576):
                        payload=bytes(i%251 for i in range(n));errors=[]
                        def send():
                            try:stream.sendall(payload)
                            except Exception as e:errors.append(e)
                        sender=threading.Thread(target=send,daemon=True);sender.start();assert exact(stream,n)==payload;sender.join(15);assert not sender.is_alive() and not errors
            if not bad and not private and proto!='http':udp_probe(proxy_port,udp_port,sizes=[1,1200,4096,8192,0,64],hosts=hosts or ['127.0.0.1'])
            status=command(server,control,'--status');assert status.returncode==0,status.stderr
            # Failed reload is transactional; the original listener stays available.
            original=(d/'server.toml').read_text();(d/'server.toml').write_text('invalid = [')
            assert command(server,control,'--reload').returncode!=0
            (d/'server.toml').write_text(original)
            assert command(server,control,'--reload').returncode==0
            assert command(server,control,'--stop').returncode==0;p.wait(timeout=5);assert not control.exists()
        except Exception as error:
            raise RuntimeError(f'{case}: {error}\nCLIENT:\n{app_log.read_text()[-4000:]}\nSERVER:\n{server_log.read_text()[-4000:]}') from error
        print('PASS',case,'bad credentials rejected' if bad else 'private target blocked' if private else ('TCP 1..1048576, reload/stop' if proto=='http' else 'TCP 1..1048576, UDP 0..8192, reload/stop'),flush=True)

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--bin-dir',type=pathlib.Path,default=ROOT/'target/debug');parser.add_argument('--case',action='append',choices=CASES);parser.add_argument('--bad-auth',action='store_true');parser.add_argument('--deny-private',action='store_true');args=parser.parse_args()
    with socketserver.ThreadingTCPServer(('127.0.0.1',0),Echo) as tcp,UdpEchoServer(('127.0.0.1',0),UdpEcho) as udp:
        tcp.daemon_threads=True
        ipv6=None
        try:ipv6=UdpEchoServer6(('::1',udp.server_address[1]),UdpEcho)
        except OSError:pass
        for server in ((tcp,udp,ipv6) if ipv6 else (tcp,udp)):threading.Thread(target=server.serve_forever,daemon=True).start()
        try:
            for case in args.case or CASES:run(case,args.bin_dir.resolve(),tcp.server_address[1],udp.server_address[1],args.bad_auth,args.deny_private,['127.0.0.1','::1','localhost'] if ipv6 else ['127.0.0.1'])
        finally:
            tcp.shutdown();udp.shutdown()
            if ipv6:ipv6.shutdown();ipv6.server_close()
if __name__=='__main__':main()
