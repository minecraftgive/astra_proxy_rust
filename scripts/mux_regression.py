#!/usr/bin/env python3
"""Reference-server mux tests with concurrent streams and physical-dial counting."""
import argparse
import concurrent.futures
import contextlib
import pathlib
import select
import socket
import socketserver
import tempfile
import threading
from protocol_matrix import ROOT, USER, EchoServer, Echo, UdpEchoServer, UdpEchoServer6, UdpEcho, exact, free_port, process, tls_material, udp_probe, wait_port, write_json, fixture
from core_regression import socks, wait_socket


class Meter(socketserver.BaseRequestHandler):
    def handle(self):
        with self.server.lock:
            self.server.connections += 1
        try:
            with socket.create_connection(('127.0.0.1', self.server.destination), timeout=15) as upstream:
                self.request.settimeout(15)
                readers = [self.request, upstream]
                while readers:
                    ready, _, _ = select.select(readers, [], [], 1)
                    for source in ready:
                        target = upstream if source is self.request else self.request
                        data = source.recv(65536)
                        if data:
                            target.sendall(data)
                        else:
                            readers.remove(source)
                            target.shutdown(socket.SHUT_WR)
        except (OSError, ValueError):
            pass


def echo_stream(proxy, target, identity):
    client, status = socks(proxy, target)
    with client:
        client.settimeout(15)
        assert status == 0, status
        payload = identity.to_bytes(4, 'big') + bytes(range(256))*256
        client.sendall(payload)
        assert exact(client, len(payload)) == payload
        client.shutdown(socket.SHUT_WR)
        assert client.recv(1) == b''


def check(protocol, mux, binaries, engines):
    with tempfile.TemporaryDirectory(prefix='astra-mux-') as td, contextlib.ExitStack() as stack:
        directory = pathlib.Path(td)
        cert, key = tls_material(directory)
        echo = stack.enter_context(EchoServer(('127.0.0.1', 0), Echo))
        udp = stack.enter_context(UdpEchoServer(('127.0.0.1', 0), UdpEcho))
        udp6 = stack.enter_context(UdpEchoServer6(('::1', udp.server_address[1]), UdpEcho))
        for listener in (echo, udp, udp6):
            threading.Thread(target=listener.serve_forever, daemon=True).start()
            stack.callback(listener.shutdown)
        peer_port, proxy_port = free_port(), free_port()
        if mux == 'muxcool':
            engine, config, _ = fixture(f'{protocol}-tls', peer_port, cert, key)
        else:
            engine = 'sing_box'
            config = {'log': {'level': 'warn'}, 'inbounds': [{'type': protocol,
                'listen': '127.0.0.1', 'listen_port': peer_port, 'users': [{'uuid': USER}],
                'tls': {'enabled': True, 'certificate_path': str(cert), 'key_path': str(key)},
                'multiplex': {'enabled': True}}], 'outbounds': [{'type': 'direct'}]}
        server_path = write_json(directory/'server.json', config)
        binary = engines/('xray' if engine == 'xray' else 'sing-box')
        peer, peer_log = stack.enter_context(process([str(binary), 'run', '-config' if engine == 'xray' else '-c', str(server_path)], directory, 'peer'))
        wait_port(peer_port, peer, peer_log)
        meter = stack.enter_context(EchoServer(('127.0.0.1', 0), Meter))
        meter.destination = peer_port
        meter.connections = 0
        meter.lock = threading.Lock()
        threading.Thread(target=meter.serve_forever, daemon=True).start()
        stack.callback(meter.shutdown)
        rules = directory/'rules.toml'
        uds = directory/'rules.sock'
        rules.write_text(f'socket="{uds}"\ndefault_action="proxy:edge"\n')
        proxy = directory/'proxy.toml'
        proxy.write_text(f'''rule_socket="{uds}"
socks5_listen="127.0.0.1:{proxy_port}"
shutdown_timeout_secs=1
[[outbounds]]
kind="native"
tag="edge"
[outbounds.config]
protocol="{protocol}"
server="127.0.0.1"
port={meter.server_address[1]}
uuid="{USER}"
mux="{mux}"
[outbounds.config.tls]
server_name="localhost"
ca_file="{directory/'ca.pem'}"
''')
        rule, rule_log = stack.enter_context(process([str(binaries/'astra-rule'), str(rules)], directory, 'rules'))
        wait_socket(uds, rule, rule_log)
        app, app_log = stack.enter_context(process([str(binaries/'astra-proxy'), str(proxy)], directory, 'proxy'))
        wait_port(proxy_port, app, app_log)
        try:
            echo_stream(proxy_port, echo.server_address[1], 0)
            for batch in range(2):
                with concurrent.futures.ThreadPoolExecutor(max_workers=16) as executor:
                    list(executor.map(lambda n: echo_stream(proxy_port, echo.server_address[1], batch*32+n), range(32)))
            before_udp = meter.connections
            assert 1 <= before_udp <= 4, f'mux did not reuse sessions: {before_udp} physical dials'
            udp_probe(proxy_port, udp.server_address[1], hosts=('127.0.0.1', '::1'))
            assert meter.connections <= 4, f'IP UDP did not reuse mux sessions: {meter.connections}'
            before_domain = meter.connections
            udp_probe(proxy_port, udp.server_address[1], hosts=('localhost',))
            if mux == 'muxcool':
                assert meter.connections == before_domain + 1, 'expected one native domain UDP fallback connection'
            else:
                assert meter.connections <= 4, f'domain UDP did not reuse mux sessions: {meter.connections}'
        except Exception as error:
            raise RuntimeError(f'{error}\n{app_log.read_text()[-5000:]}\n{peer_log.read_text()[-3000:]}') from error
        print(f'PASS {protocol}/{mux}: 65 TCP streams, UDP IPv4/IPv6/domain, {meter.connections} physical connections', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT/'target/release')
    parser.add_argument('--engine-dir', type=pathlib.Path, default=ROOT/'.test-tools/bin')
    parser.add_argument('--protocol', action='append', choices=['vless', 'vmess'])
    parser.add_argument('--mux', action='append', choices=['smux', 'yamux', 'h2mux', 'muxcool'])
    args = parser.parse_args()
    failures = []
    for protocol in args.protocol or ['vless', 'vmess']:
        for mux in args.mux or ['smux', 'yamux', 'h2mux', 'muxcool']:
            try:
                check(protocol, mux, args.bin_dir.resolve(), args.engine_dir.resolve())
            except Exception as error:
                failures.append((protocol, mux))
                print(f'FAIL {protocol}/{mux}: {error}', flush=True)
    if failures:
        raise SystemExit(f'Failed mux cases: {failures}')


if __name__ == '__main__':
    main()
