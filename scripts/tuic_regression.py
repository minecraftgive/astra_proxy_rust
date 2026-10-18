#!/usr/bin/env python3
"""TUIC native QUIC reuse, restart and authentication/certificate regressions."""
import concurrent.futures
import contextlib
import argparse
import json
import os
import pathlib
import socket
import tempfile
import threading
import time
from protocol_matrix import ROOT, USER, PASSWORD, Echo, EchoServer, UdpEcho, UdpEchoServer, UdpEchoServer6, exact, fixture, free_port, process, tls_material, udp_probe, wait_port, write_json
from core_regression import socks, wait_socket


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT / 'target/debug')
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    os.environ['RUST_LOG'] = 'astra_proxy=debug'
    with tempfile.TemporaryDirectory(prefix='astra-tuic-') as td, contextlib.ExitStack() as stack:
        directory = pathlib.Path(td)
        cert, key = tls_material(directory)
        echo = stack.enter_context(EchoServer(('127.0.0.1',0), Echo))
        destination = echo.server_address[1]
        udp4 = stack.enter_context(UdpEchoServer(('127.0.0.1',destination), UdpEcho))
        udp6 = stack.enter_context(UdpEchoServer6(('::1',destination), UdpEcho))
        for server in (echo,udp4,udp6):
            threading.Thread(target=server.serve_forever,daemon=True).start()
        peer_port, proxy_port = free_port(), free_port()
        _, server_config, _ = fixture('sing-tuic',peer_port,cert,key)
        server_config['log']['level'] = 'warn'
        server_path = write_json(directory/'server.json',server_config)
        server_command = [str(ROOT/'.test-tools/bin/sing-box'),'run','-c',str(server_path)]
        peer, peer_log = stack.enter_context(process(server_command,directory,'peer'))
        time.sleep(.3)
        assert peer.poll() is None
        rule_path, config_path = directory/'rule.toml',directory/'proxy.toml'
        rule_path.write_text(f'socket="{directory}/rules.sock"\ndefault_action="proxy:edge"\n')
        config = f'''rule_socket="{directory}/rules.sock"
socks5_listen="127.0.0.1:{proxy_port}"
connect_timeout_secs=6
shutdown_timeout_secs=1
[[outbounds]]
kind="native"
tag="edge"
[outbounds.config]
protocol="tuic"
server="127.0.0.1"
port={peer_port}
uuid="{USER}"
password="{PASSWORD}"
[outbounds.config.tls]
server_name="localhost"
ca_file="{directory}/ca.pem"
'''
        config_path.write_text(config)
        rule, log = stack.enter_context(process([str(binaries/'astra-rule'),str(rule_path)],directory,'rule'))
        wait_socket(directory/'rules.sock',rule,log)
        def transfer(index):
            connection, status = socks(proxy_port,destination)
            with connection:
                assert status == 0, status
                data = bytes([index]) * 65536
                connection.sendall(data)
                assert exact(connection,len(data)) == data
                connection.shutdown(socket.SHUT_WR)
                assert connection.recv(1) == b''
        with process([str(binaries/'astra-proxy'),str(config_path)],directory,'proxy') as (app, log):
            wait_port(proxy_port,app,log)
            try:
                with concurrent.futures.ThreadPoolExecutor(max_workers=32) as pool:
                    for _ in range(20):
                        list(pool.map(transfer,range(32)))
            except Exception as error:
                raise RuntimeError(str(error) + '\nPROXY\n' + log.read_text()[-8000:] + '\nSERVER\n' + peer_log.read_text()[-12000:]) from error
            udp_probe(proxy_port,destination,edge=True)
            assert log.read_text().count('TUIC QUIC session established') == 1, log.read_text()
            print('PASS TUIC 20 batches of 32 concurrent TCP streams + UDP share one QUIC connection',flush=True)
            peer.terminate(); peer.wait(timeout=5)
            new_peer, _ = stack.enter_context(process(server_command,directory,'peer-restarted'))
            time.sleep(.3)
            assert new_peer.poll() is None
            deadline = time.monotonic()+20
            while True:
                try:
                    transfer(9)
                    break
                except (AssertionError,OSError,EOFError):
                    if time.monotonic()>deadline: raise
                    time.sleep(.1)
            assert log.read_text().count('TUIC QUIC session established') >= 2
            print('PASS TUIC reconnects after server restart',flush=True)
        for name, changed in [
            ('password',config.replace(PASSWORD,'wrong-password')),
            ('server-name',config.replace('server_name="localhost"','server_name="wrong.example"')),
            ('untrusted-certificate',config.replace(f'ca_file="{directory}/ca.pem"','')),
        ]:
            config_path.write_text(changed)
            with process([str(binaries/'astra-proxy'),str(config_path)],directory,name) as (app,log):
                wait_port(proxy_port,app,log)
                connection, status = socks(proxy_port,destination)
                with connection:
                    if status == 0:
                        # TUIC has no authentication ACK: a bad password may fail on first I/O.
                        try:
                            connection.sendall(b'must-not-relay')
                            assert connection.recv(32) == b'', f'{name}: relayed despite invalid credentials'
                        except (ConnectionResetError,BrokenPipeError):
                            pass
                    else:
                        assert status == 5, status
                print('PASS TUIC rejects '+name,flush=True)
        for server in (echo,udp4,udp6): server.shutdown()

if __name__=='__main__': main()
