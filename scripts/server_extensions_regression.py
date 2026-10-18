#!/usr/bin/env python3
"""Server extension interoperability; server and client fixtures are separate."""
import os,argparse,base64,contextlib,concurrent.futures,hashlib,json,pathlib,socket,socketserver,ssl,subprocess,tempfile,threading,time
from protocol_matrix import Echo,UdpEcho,UdpEchoServer,UdpEchoServer6,TlsEcho,exact,free_port,process,wait_port,udp_probe,tls_probe
from server_regression import ROOT,USER,PASSWORD,connect,command
from platform_support import executable
CASES=['reality','vision','vision-reality','reality-grpc','reality-xhttp',
       'mux-smux','mux-yamux','mux-h2mux','mux-muxcool',
       'vmess-mux-smux','vmess-mux-yamux','vmess-mux-h2mux',
       'decrypt-native','decrypt-xorpub','decrypt-random','decrypt-mlkem768','decrypt-hybrid','decrypt-0rtt',
       'vision-decrypt','vision-decrypt-ws','vision-decrypt-grpc','xhttp-stream-up','xhttp-packet-up','salamander',
       'reality-xhttp-stream-up','reality-xhttp-packet-up','decrypt-xhttp-stream-one','decrypt-xhttp-stream-up','decrypt-xhttp-packet-up',
       'vision-decrypt-xhttp-stream-one','vision-decrypt-xhttp-stream-up','vision-decrypt-xhttp-packet-up','vision-reality-decrypt']

def run(case,bins,bad=False,fault=None):
    with tempfile.TemporaryDirectory(prefix='astra-server-extension-') as temp,contextlib.ExitStack() as stack:
        d=pathlib.Path(temp);sd=d/'server';cd=d/'client';sd.mkdir();cd.mkdir()
        binary=bins/executable('astra-server-core');proxy=bins/executable('astra-proxy')
        subprocess.run([str(binary),'--generate-cert',str(sd/'tls'),'localhost'],check=True,stdout=subprocess.DEVNULL)
        cert=sd/'tls/cert.pem';key=sd/'tls/key.pem';control=sd/'control.sock'
        tls=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);tls.load_cert_chain(cert,key)
        echo=stack.enter_context(socketserver.ThreadingTCPServer(('127.0.0.1',0),Echo));echo.daemon_threads=True
        secure=stack.enter_context(socketserver.ThreadingTCPServer(('127.0.0.1',0),TlsEcho));secure.daemon_threads=True;secure.tls=tls;secure.errors=[]
        udp=stack.enter_context(UdpEchoServer(('127.0.0.1',0),UdpEcho));udp6=stack.enter_context(UdpEchoServer6(('::1',udp.server_address[1]),UdpEcho))
        for s in (echo,secure,udp,udp6):threading.Thread(target=s.serve_forever,daemon=True).start();stack.callback(s.shutdown)
        proto='vmess' if case.startswith('vmess-') else 'trojan' if case.startswith('trojan-') else 'shadowsocks' if case.startswith('ss-') else 'hysteria2' if case=='salamander' else 'vless'
        vision=case.startswith('vision');reality='reality' in case;decrypt='decrypt' in case;mux=case.split('-mux-')[-1] if '-mux-' in case else case.removeprefix('mux-') if case.startswith('mux-') else None
        padding=mux and mux.endswith('-padding');mux=mux.removesuffix('-padding') if mux else None
        transport='ws' if case.endswith('-ws') else 'grpc' if case.endswith('-grpc') else 'xhttp' if 'xhttp' in case else 'tcp'
        peer,local=free_port(),free_port()
        server=f'allow_private_targets=true\nshutdown_timeout_secs=0\n[[listeners]]\ntag="server-inbound"\nprotocol="{proto}"\nlisten="127.0.0.1:{peer}"\n'
        client=f'protocol="{proto}"\nserver="127.0.0.1"\nport={peer}\n'
        if proto in ('vless','vmess'):client+=f'uuid="{USER if not bad else str(__import__("uuid").uuid4())}"\n'
        if proto in ('trojan','hysteria2'):client+=f'password="{PASSWORD if not bad else "incorrect-password"}"\n'
        if proto=='shadowsocks':server+=f'cipher="aes-128-gcm"\npassword="{PASSWORD}"\n';client+=f'cipher="aes-128-gcm"\npassword="{PASSWORD if not bad else "incorrect-password"}"\n'
        if vision:server+='flow="xtls-rprx-vision"\n';client+='flow="xtls-rprx-vision"\n'
        if mux:
            server+='multiplex=true\n';client+=f'mux="{mux}"\n'
            if padding:client+='mux_padding=true\n'
        if case=='salamander':server+=f'obfs_password="{PASSWORD}"\n';client+=f'obfs_password="{PASSWORD}"\npin_sha256="{hashlib.sha256(ssl.PEM_cert_to_DER_cert(cert.read_text())).hexdigest()}"\n'
        if transport!='tcp':
            server+=f'transport="{transport}"\npath="/astra-extension"\nservice_name="astra-extension"\n'
            client+=f'[outbounds.config.transport]\nkind="{transport}"\npath="/astra-extension"\nservice_name="astra-extension"\n'
            if transport=='xhttp':client+=f'mode="{case.split("xhttp-",1)[1] if "xhttp-" in case else "stream-one"}"\n'
        if decrypt:
            kind='mlkem768' if case=='decrypt-mlkem768' else 'hybrid' if case=='decrypt-hybrid' else 'x25519'
            keys=json.loads(subprocess.check_output([str(binary),'--generate-decryption-keys',kind],text=True));mode=case.removeprefix('decrypt-') if case in ('decrypt-xorpub','decrypt-random') else 'native';rtt='0rtt' if case=='decrypt-0rtt' else '1rtt'
            # Client has public keys only; no server secret is copied across.
            encryption=f'mlkem768x25519plus.{mode}.{rtt}.'+'.'.join(keys['client_public_keys'])
            client=client.replace(f'port={peer}\n',f'port={peer}\nencryption="{encryption}"\n')
            server+=f'[listeners.decryption]\nprivate_keys={json.dumps(keys["server_private_keys"])}\nmode="{mode}"\nticket_lifetime_secs={300 if rtt=="0rtt" else 0}\n'
        if reality:
            keys=json.loads(subprocess.check_output([str(binary),'--generate-reality-keys'],text=True))
            server+=f'[listeners.reality]\nprivate_key="{keys["server_private_key"]}"\nserver_names=["localhost"]\nshort_ids=["0123456789abcdef"]\ntarget="127.0.0.1:{secure.server_address[1]}"\n'
            client+=f'[outbounds.config.tls]\nserver_name="localhost"\nreality_public_key="{keys["client_public_key"]}"\nreality_short_id="0123456789abcdef"\nfingerprint="chrome"\n'
        elif proto!='shadowsocks':
            server+=f'[listeners.tls]\ncertificate="{cert.as_posix()}"\nprivate_key="{key.as_posix()}"\n'
            client+='[outbounds.config.tls]\nserver_name="localhost"\n'
            if proto!='hysteria2':client+=f'ca_file="{cert.as_posix()}"\n'
        if proto!='shadowsocks':server+=f'[[listeners.users]]\nname="server-user"\nuuid="{USER}"\npassword="{PASSWORD}"\n'
        sp=sd/'server.toml';cp=cd/'client.toml';sp.write_text(server);cp.write_text(f'standalone=true\nstandalone_outbound="remote-server"\nsocks5_listen="127.0.0.1:{local}"\nshutdown_timeout_secs=0\n[[outbounds]]\nkind="native"\ntag="remote-server"\n[outbounds.config]\n'+client)
        peer_process,peer_log=stack.enter_context(process([str(binary),'--config',str(sp),'--control',str(control)],sd,'core'))
        for _ in range(100):
            if control.exists() and command(binary,control,'--status').returncode==0:break
            if peer_process.poll() is not None:raise RuntimeError(peer_log.read_text())
            time.sleep(.02)
        if fault=='reality-key':
            assert reality
            other=json.loads(subprocess.check_output([str(binary),'--generate-reality-keys'],text=True))
            client=client.replace(keys['client_public_key'],other['client_public_key'])
        elif fault=='short-id':client=client.replace('reality_short_id="0123456789abcdef"','reality_short_id="1111111111111111"')
        elif fault=='decryption-key':
            assert decrypt
            other=json.loads(subprocess.check_output([str(binary),'--generate-decryption-keys','x25519'],text=True))
            client=client.replace(encryption,'mlkem768x25519plus.native.1rtt.'+other['client_public_keys'][0])
        elif fault=='salamander-password':client=client.replace(f'obfs_password="{PASSWORD}"','obfs_password="incorrect-obfs-password"')
        if fault:cp.write_text(f'standalone=true\nstandalone_outbound="remote-server"\nsocks5_listen="127.0.0.1:{local}"\nshutdown_timeout_secs=0\n[[outbounds]]\nkind="native"\ntag="remote-server"\n[outbounds.config]\n'+client)
        assert subprocess.run([str(binary),'--config',str(cp),'--check'],capture_output=True,timeout=5).returncode!=0,'server accepted a client configuration'
        assert subprocess.run([str(proxy),'--check',str(sp)],capture_output=True,timeout=5).returncode!=0,'client accepted a server configuration'
        app,app_log=stack.enter_context(process([str(proxy),str(cp)],cd,'proxy'));wait_port(local,app,app_log)
        try:
            if bad or fault:
                stream,status=connect(local,echo.server_address[1])
                with stream:
                    if status==0:
                        stream.sendall(b'must-not-forward')
                        try:response=stream.recv(16)
                        except OSError:response=b''
                        assert response!=b'must-not-forward'
            else:
                def check(identity):
                    stream,status=connect(local,echo.server_address[1])
                    with stream:
                        assert status==0,status
                        for n in (1,4096,65536):
                            payload=bytes([identity%251])*n;stream.sendall(payload);assert exact(stream,n)==payload
                check(1)
                if mux:
                    with concurrent.futures.ThreadPoolExecutor(max_workers=16)as pool:
                        for batch in range(3):list(pool.map(check,range(batch*32,(batch+1)*32)))
                if vision:
                    tls_probe(local,secure.server_address[1],2*1024*1024,cert)
                    if transport=='tcp':
                        text=peer_log.read_text();assert 'inbound DIRECT' in text and 'outbound DIRECT' in text,'Vision did not switch both directions'
                udp_probe(local,udp.server_address[1],sizes=([1,1200,4096,8192] if mux=="muxcool" else [0,1,1200,4096,8192]),hosts=['127.0.0.1','::1','localhost'])
                if mux=='muxcool':empty_cool(peer,udp.server_address[1],cert)
                if case=='decrypt-0rtt':check(2);check(3)
                if reality:
                    context=ssl.create_default_context(cafile=str(cert))
                    with socket.create_connection(('127.0.0.1',peer),timeout=5)as raw:
                        with context.wrap_socket(raw,server_hostname='localhost')as normal:normal.sendall(b'cover');assert exact(normal,5)==b'cover'
            assert command(binary,control,'--stop').returncode==0;peer_process.wait(timeout=5)
        except Exception as error:raise RuntimeError(f'{case}: {error}\nCLIENT:\n{app_log.read_text()[-5000:]}\nSERVER:\n{peer_log.read_text()[-5000:]}')from error
        print('PASS',case,'invalid credentials/extension key rejected' if bad or fault else 'TCP/UDP'+(' + 96 streams in concurrent batches' if mux else '')+(' + inner TLS 1.3 2MiB' if vision else '')+(' + cover fallback' if reality else ''),flush=True)

def empty_cool(peer,target,cert):
    context=ssl.create_default_context(cafile=str(cert))
    with socket.create_connection(('127.0.0.1',peer),timeout=5)as raw:
        with context.wrap_socket(raw,server_hostname='localhost')as stream:
            stream.sendall(bytes([0])+__import__('uuid').UUID(USER).bytes+bytes([0,3]));assert exact(stream,2)==bytes([0,0])
            meta=bytes([0,1,1,0,2])+target.to_bytes(2,'big')+bytes([1,127,0,0,1])
            stream.sendall(len(meta).to_bytes(2,'big')+meta+bytes([0,4,0,1,2,1,0,0]))
            n=int.from_bytes(exact(stream,2),'big');reply=exact(stream,n);assert reply[:4]==bytes([0,1,2,1]);assert exact(stream,2)==bytes([0,0])

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--bin-dir',type=pathlib.Path,default=ROOT/'target/debug');parser.add_argument('--case',action='append',choices=CASES);parser.add_argument('--bad-auth',action='store_true');parser.add_argument('--fault',choices=['reality-key','short-id','decryption-key','salamander-password']);args=parser.parse_args();os.environ['ASTRA_SERVER_DEBUG']='1'
    for case in args.case or CASES:run(case,args.bin_dir.resolve(),args.bad_auth,args.fault)
if __name__=='__main__':main()
