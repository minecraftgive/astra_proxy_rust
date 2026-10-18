#!/usr/bin/env python3
"""Standalone TCP/HTTP/UDP must use the selected upstream without any rule IPC."""
import argparse
import contextlib
import pathlib
import socket
import subprocess
import sys
import tempfile
import threading
from protocol_matrix import ROOT, Echo, EchoServer, UdpEcho, UdpEchoServer, free_port, process, wait_port, fixture, write_json, exact, udp_probe
from core_regression import socks
from mux_regression import Meter


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir',type=pathlib.Path,default=ROOT/'target/debug')
    args=parser.parse_args()
    binaries=args.bin_dir.resolve()
    with tempfile.TemporaryDirectory(prefix='astra-standalone-') as td,contextlib.ExitStack() as stack:
        d=pathlib.Path(td)
        # A listening Unix socket detects accidental queries even when a rule
        # socket is supplied. No rule-core executable is started by this test.
        trap=stack.enter_context(socket.socket(socket.AF_UNIX,socket.SOCK_STREAM))
        trap.bind(str(d/'unused-rule.sock'));trap.listen(8);trap.settimeout(.1)
        queries=[];stopped=threading.Event()
        def observe():
            while not stopped.is_set():
                try:
                    conn,_=trap.accept();queries.append(True);conn.close()
                except socket.timeout:pass
        watcher=threading.Thread(target=observe,daemon=True);watcher.start()
        peer_port=free_port()
        _,server,_=fixture('socks-plain',peer_port,None,None)
        peer,peer_log=stack.enter_context(process([str(ROOT/'.test-tools/bin/xray'),'run','-config',str(write_json(d/'peer.json',server))],d,'peer'))
        wait_port(peer_port,peer,peer_log)
        meter=stack.enter_context(EchoServer(('127.0.0.1',0),Meter))
        meter.destination=peer_port;meter.connections=0;meter.lock=threading.Lock()
        echo=stack.enter_context(EchoServer(('127.0.0.1',0),Echo))
        udp=stack.enter_context(UdpEchoServer(('127.0.0.1',0),UdpEcho))
        for listener in (meter,echo,udp):
            threading.Thread(target=listener.serve_forever,daemon=True).start()
            stack.callback(listener.shutdown)
        # Launcher must work even if there is no astra-rule binary or rule file.
        only_proxy=d/'bin';only_proxy.mkdir();(only_proxy/'astra-proxy').symlink_to(binaries/'astra-proxy')
        for multiple in (False,True):
            sp,hp=free_port(),free_port()
            config=d/'proxy.toml'
            config.write_text(f'''standalone=true
socks5_listen="127.0.0.1:{sp}"
http_listen="127.0.0.1:{hp}"
shutdown_timeout_secs=1
'''+('standalone_outbound="selected"\n' if multiple else f'rule_socket="{d / "unused-rule.sock"}"\n')+f'''
[[outbounds]]
kind="socks5"
tag="selected"
server="127.0.0.1:{meter.server_address[1]}"
'''+('''[[outbounds]]
kind="socks5"
tag="do-not-select"
server="127.0.0.1:1"
''' if multiple else ''))
            launcher=[sys.executable,str(ROOT/'scripts/astra.py'),'--bin-dir',str(only_proxy),'--proxy',str(config),'--rules',str(d/'no-rule-file.toml')]
            subprocess.run(launcher+['--check'],check=True,stdout=subprocess.DEVNULL)
            before=meter.connections
            with process(launcher,d,f'proxy-{multiple}') as (app,log):
                wait_port(sp,app,log)
                client,status=socks(sp,echo.server_address[1])
                with client:
                    assert status==0,status
                    client.sendall(b'standalone-socks');assert exact(client,16)==b'standalone-socks'
                with socket.create_connection(('127.0.0.1',hp),timeout=3) as client:
                    client.sendall(f'CONNECT 127.0.0.1:{echo.server_address[1]} HTTP/1.1\r\n\r\n'.encode())
                    header=b''
                    while not header.endswith(b'\r\n\r\n'):header+=exact(client,1)
                    assert header.startswith(b'HTTP/1.1 200')
                    client.sendall(b'http');assert exact(client,4)==b'http'
                udp_probe(sp,udp.server_address[1],sizes=(1,1200,4096),hosts=('127.0.0.1',))
                assert meter.connections-before==3,'traffic did not use upstream for both TCP listeners and UDP'
                assert not queries,'standalone contacted Rule Core'
            print('PASS standalone '+('explicit node among multiple' if multiple else 'single node')+': SOCKS/HTTP/UDP, upstream verified, no rule IPC/binary/config',flush=True)
        # A broken upstream must fail, even though the local target is reachable.
        sp=free_port();config=d/'proxy.toml'
        config.write_text(f'standalone=true\nsocks5_listen="127.0.0.1:{sp}"\nshutdown_timeout_secs=1\n[[outbounds]]\nkind="socks5"\ntag="offline"\nserver="127.0.0.1:{free_port()}"\n')
        with process([str(binaries/'astra-proxy'),str(config)],d,'offline') as (app,log):
            wait_port(sp,app,log);client,status=socks(sp,echo.server_address[1]);client.close();assert status==5,status
        stopped.set();watcher.join(timeout=1)
        assert not queries
        print('PASS standalone upstream outage fails closed (no direct fallback)',flush=True)


if __name__=='__main__':main()
