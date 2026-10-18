#!/usr/bin/env python3
"""Reproduce the first-release functional gate; record commands, status and output."""
import argparse
import hashlib
import json
import pathlib
import subprocess
import sys
import time
from protocol_matrix import ROOT


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT/'target/release')
    parser.add_argument('--tls-python', default=sys.executable, help='Python with TLS 1.3 for the Vision payload gate')
    parser.add_argument('--output', type=pathlib.Path, default=ROOT/'docs/release-verification.json')
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    matrix = [sys.executable, 'scripts/protocol_matrix.py', '--bin-dir', str(binaries)]
    def cases(names):
        return [part for name in names for part in ('--case',name)]
    steps = [
        ('workspace tests', ['cargo','test','--workspace']),
        ('workspace lint', ['cargo','clippy','--workspace','--all-targets','--','-D','warnings']),
        ('VMess codec patches', ['cargo','test','--manifest-path','vendor/meow-proxy/Cargo.toml','--no-default-features','--features','vmess,mux','--lib','vmess::']),
        ('Hysteria2 codec patches', ['cargo','test','--manifest-path','vendor/meow-proxy/Cargo.toml','--no-default-features','--features','hysteria2','--lib','hysteria2::']),
        ('TCP matrix', matrix),
        ('UDP basic matrix', matrix+['--udp']+cases(['socks-plain','socks-auth','ss-plain','sing-shadowsocks','vmess-tls','trojan-tls','vless-tls','vless-ws','vless-grpc','vless-httpupgrade','vless-xhttp:stream-one','vless-xhttp:stream-up','vless-xhttp:packet-up','vless-reality','sing-hysteria2','sing-tuic'])),
        ('UDP full edge peers', matrix+['--udp-edge']+cases(['sing-socks-plain','sing-socks-auth','sing-trojan','sing-vless','sing-vmess','sing-shadowsocks-aead','sing-shadowsocks'])),
        ('TUIC datagram edges', matrix+['--udp-edge','--case','sing-tuic']),
        ('TUIC stream edges', matrix+['--udp-edge','--case','sing-tuic','--tuic-udp-mode','quic']),
        ('Hysteria2 empty and 4096', matrix+['--udp-size','0','--udp-size','4096','--case','sing-hysteria2']),
        ('Hysteria2 Salamander TCP', matrix+['--case','sing-hysteria2','--hy2-obfs']),
        ('Hysteria2 Salamander UDP', matrix+['--case','sing-hysteria2','--hy2-obfs','--udp-size','0','--udp-size','4096']),
        ('native credential rejection', matrix+['--fault','credential']+cases(['vless-tls','vmess-tls','trojan-tls','ss-plain','sing-shadowsocks','sing-hysteria2','sing-tuic'])),
        ('SNI rejection', matrix+['--fault','server-name']+cases(['vless-tls','vmess-tls','trojan-tls','sing-tuic'])),
        ('untrusted CA rejection', matrix+['--fault','untrusted-ca']+cases(['vless-tls','vmess-tls','trojan-tls','sing-tuic'])),
        ('Hysteria2 pin rejection', matrix+['--fault','pin','--case','sing-hysteria2']),
        ('upstream credential rejection', matrix+['--bad-auth','--case','socks-auth','--case','http-auth']),
        ('REALITY key rejection', matrix+['--bad-reality','--case','vless-reality']),
        ('Vision TLS 1.3 raw switch', [args.tls_python]+matrix[1:]+['--case','vless-vision','--tls-payload','--payload-bytes','8388608']),
        ('native half-close with sing-box', matrix+['--trailer','--payload-bytes','8388608']+cases(['sing-vless','sing-vmess','sing-tuic'])),
        ('Xray half-close isolation', [sys.executable,'scripts/half_close_diagnostic.py']),
    ]
    for script in ('core_regression','udp_regression','tuic_regression','mux_regression'):
        steps.append((script,[sys.executable,f'scripts/{script}.py','--bin-dir',str(binaries)]))
    report = {'scope':'macOS arm64 first-release functional gate; known peer/mode limitations documented separately',
              'binary_sha256': {name:hashlib.sha256((binaries/name).read_bytes()).hexdigest() for name in ('astra-rule','astra-proxy','astra-tui')}, 'steps':[], 'passed':False}
    args.output.parent.mkdir(parents=True,exist_ok=True)
    for label, command in steps:
        print('RUN',label,flush=True)
        start=time.monotonic()
        try:
            result=subprocess.run(command,cwd=ROOT,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,timeout=300)
            code, output=result.returncode,result.stdout
        except subprocess.TimeoutExpired as error:
            code, output=124,str(error)
        report['steps'].append({'name':label,'command':command,'exit_code':code,'seconds':round(time.monotonic()-start,2),'output':output})
        args.output.write_text(json.dumps(report,indent=2)+'\n')
        print('PASS' if code==0 else 'FAIL',label,flush=True)
        if code!=0:
            print(output[-5000:],flush=True)
            raise SystemExit(code)
    report['passed']=True
    args.output.write_text(json.dumps(report,indent=2)+'\n')
    print('PASS all release functional gates',flush=True)


if __name__=='__main__':
    main()
