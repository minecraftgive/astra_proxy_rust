#!/usr/bin/env python3
"""Hot switch/reload: existing TCP survives, new traffic switches, invalid candidates roll back."""
import argparse
import contextlib
import pathlib
import signal
import socket
import subprocess
import tempfile
import threading
import time
from protocol_matrix import ROOT, Echo, EchoServer, free_port, process, wait_port, fixture, write_json, exact
from core_regression import socks, wait_socket
from mux_regression import Meter


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir',type=pathlib.Path,default=ROOT/'target/debug')
    args=parser.parse_args(); binaries=args.bin_dir.resolve()
    with tempfile.TemporaryDirectory(prefix='astra-reload-') as td, contextlib.ExitStack() as stack:
        d=pathlib.Path(td)
        echo=stack.enter_context(EchoServer(('127.0.0.1',0),Echo))
        threading.Thread(target=echo.serve_forever,daemon=True).start();stack.callback(echo.shutdown)
        destination=echo.server_address[1]
        peer_port=free_port();_,server,_=fixture('socks-plain',peer_port,None,None)
        peer,log=stack.enter_context(process([str(ROOT/'.test-tools/bin/xray'),'run','-config',str(write_json(d/'peer.json',server))],d,'peer'))
        wait_port(peer_port,peer,log)
        meters=[]
        for _ in range(2):
            meter=stack.enter_context(EchoServer(('127.0.0.1',0),Meter));meter.destination=peer_port;meter.connections=0;meter.lock=threading.Lock()
            threading.Thread(target=meter.serve_forever,daemon=True).start();stack.callback(meter.shutdown);meters.append(meter)
        sp=free_port();config=d/'proxy.toml';control=d/'proxy-control.sock'
        def text(selected='a',port=sp,watch=False):
            return f'''standalone=true
standalone_outbound="{selected}"
socks5_listen="127.0.0.1:{port}"
watch_config={str(watch).lower()}
shutdown_timeout_secs=1
[[outbounds]]
kind="socks5"
tag="a"
server="127.0.0.1:{meters[0].server_address[1]}"
[[outbounds]]
kind="socks5"
tag="b"
server="127.0.0.1:{meters[1].server_address[1]}"
'''
        def reload(binary='astra-proxy',path=None,ok=True,sock=control):
            command=[str(binaries/binary),'--switch' if path else '--reload',str(sock)]+([str(path)] if path else [])
            result=subprocess.run(command,capture_output=True,text=True,timeout=10)
            assert (result.returncode==0)==ok,result.stdout+result.stderr
        def probe(port=sp,expected=0):
            client,status=socks(port,destination)
            with client:
                assert status==expected,(status,expected)
                if status==0:client.sendall(b'echo');assert exact(client,4)==b'echo'
        config.write_text(text())
        with process(['env',f'ASTRA_CONTROL_SOCKET={control}',str(binaries/'astra-proxy'),str(config)],d,'proxy') as (app,log):
            wait_port(sp,app,log);wait_socket(control,app,log)
            old,status=socks(sp,destination);assert status==0
            with old:
                old.sendall(b'old1');assert exact(old,4)==b'old1'
                replacement=d/'selected-b.toml';replacement.write_text(text('b'))
                reload(path=replacement);probe();assert meters[1].connections==1
                old.sendall(b'old2');assert exact(old,4)==b'old2'
                replacement.write_text('invalid=[');reload(ok=False);probe()
                blocked=socket.socket();blocked.bind(('127.0.0.1',0));blocked.listen()
                with blocked:
                    replacement.write_text(text('a',blocked.getsockname()[1]));reload(ok=False);probe()
                # A successful reload keeps the switched file as its active path.
                replacement.write_text(text('a'));app.send_signal(signal.SIGHUP)
                for _ in range(50):
                    count=meters[0].connections;probe()
                    if meters[0].connections>count:break
                    time.sleep(.02)
                else:raise AssertionError('SIGHUP did not reload switched path')
                replacement.write_text(text('a',watch=True));reload()
                replacement.write_text(text('b',watch=True))
                time.sleep(1.2);count=meters[1].connections;probe();assert meters[1].connections==count+1
                new_port=free_port();replacement.write_text(text('b',new_port));reload();probe(new_port)
                with contextlib.suppress(ConnectionRefusedError):
                    socket.create_connection(('127.0.0.1',sp),timeout=1).close();raise AssertionError('old listener still accepting')
                replacement.write_text(text('b',new_port).replace('socks5_listen','http_listen'));reload()
                with socket.create_connection(('127.0.0.1',new_port),timeout=3) as http:
                    http.sendall(f'CONNECT 127.0.0.1:{destination} HTTP/1.1\r\n\r\n'.encode())
                    header=b''
                    while not header.endswith(b'\r\n\r\n'):header+=exact(http,1)
                    assert header.startswith(b'HTTP/1.1 200')
                    http.sendall(b'role');assert exact(http,4)==b'role'
                old.sendall(b'old3');assert exact(old,4)==b'old3'
            print('PASS proxy: switch path/node/listener, SIGHUP, file watch, parse/bind rollback, old TCP retained',flush=True)
        rule_config=d/'rules.toml';rule_socket=d/'rules.sock';rule_control=d/'rule-control.sock'
        def rules(action='direct',watch=False,sock=rule_socket):return f'socket="{sock}"\nwatch_config={str(watch).lower()}\ndefault_action="{action}"\n'
        rule_config.write_text(rules());config.write_text(f'rule_socket="{rule_socket}"\nsocks5_listen="127.0.0.1:{sp}"\ndecision_cache_ttl_secs=600\nshutdown_timeout_secs=1\n')
        with process(['env',f'ASTRA_CONTROL_SOCKET={rule_control}',str(binaries/'astra-rule'),str(rule_config)],d,'rule') as (rule,rule_log), process(['env',f'ASTRA_CONTROL_SOCKET={control}',str(binaries/'astra-proxy'),str(config)],d,'proxy-rules') as (app,log):
            wait_socket(rule_socket,rule,rule_log);wait_socket(rule_control,rule,rule_log);wait_port(sp,app,log);time.sleep(.15)
            old,status=socks(sp,destination);assert status==0
            with old:
                probe();rule_config.write_text(rules('block'));reload('astra-rule',sock=rule_control)
                time.sleep(.1);probe(expected=2)
                old.sendall(b'alive');assert exact(old,5)==b'alive'
                rule_config.write_text('broken=[');reload('astra-rule',ok=False,sock=rule_control);probe(expected=2)
                next_file=d/'next-rules.toml';next_file.write_text(rules('direct',watch=True));reload('astra-rule',path=next_file,sock=rule_control);time.sleep(.1);probe()
                next_file.write_text(rules('block',watch=True));time.sleep(1.2);probe(expected=2)
                # Atomically rebinding the rule socket does not kill existing IPC peers.
                next_socket=d/'next-rules.sock';next_file.write_text(rules('direct',sock=next_socket));reload('astra-rule',sock=rule_control)
                assert next_socket.exists() and not rule_socket.exists()
                config.write_text(config.read_text().replace(str(rule_socket),str(next_socket)));reload();probe()
            print('PASS rule: active IPC sees new rules, 600-second cache invalidated, switch/watch/socket, rollback, old TCP retained',flush=True)
        assert not rule_socket.exists() and not control.exists()

if __name__=='__main__':main()
