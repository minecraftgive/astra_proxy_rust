#!/usr/bin/env python3
"""UDP association routing, endpoint isolation, limits and lifecycle regressions."""
import contextlib
import argparse
import pathlib
import socket
import tempfile
import threading
import time
from protocol_matrix import ROOT, UdpEcho, UdpEchoServer, exact, free_port, process, wait_port
from core_regression import wait_socket, socks


def association(port, source_port=0):
    control = socket.create_connection(('127.0.0.1', port), timeout=3)
    control.sendall(b'\x05\x01\x00')
    assert exact(control, 2) == b'\x05\x00'
    control.sendall(b'\x05\x03\x00\x01' + bytes(4) + source_port.to_bytes(2, 'big'))
    reply = exact(control, 10)
    assert reply[:4] == b'\x05\x00\x00\x01', reply
    return control, (socket.inet_ntoa(reply[4:8]), int.from_bytes(reply[8:], 'big'))


def packet(port, payload=b'udp'):
    return b'\x00\x00\x00\x01\x7f\x00\x00\x01' + port.to_bytes(2, 'big') + payload


def silent(sock):
    try:
        data = sock.recv(65536)
    except (socket.timeout, TimeoutError, ConnectionRefusedError):
        return
    raise AssertionError(f'unexpected UDP response: {data!r}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=pathlib.Path, default=ROOT / 'target/debug')
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    with tempfile.TemporaryDirectory(prefix='astra-udp-') as td, contextlib.ExitStack() as stack:
        directory = pathlib.Path(td)
        echoes = [stack.enter_context(UdpEchoServer(('127.0.0.1', 0), UdpEcho)) for _ in range(2)]
        for echo in echoes:
            threading.Thread(target=echo.serve_forever, daemon=True).start()
        ports = [echo.server_address[1] for echo in echoes]
        rule_path, proxy_path, rule_socket = directory/'rule.toml', directory/'proxy.toml', directory/'rule.sock'
        rule_path.write_text(f'socket="{rule_socket}"\ndefault_action="block"\n[[rules]]\nkind="network"\nvalue="udp"\naction="direct"\n')
        port = free_port()
        proxy_path.write_text(f'rule_socket="{rule_socket}"\nsocks5_listen="127.0.0.1:{port}"\nmax_udp_flows=1\nudp_targets_per_association=1\nudp_idle_timeout_secs=2\nshutdown_timeout_secs=1\n')
        rule, log = stack.enter_context(process([str(binaries/'astra-rule'),str(rule_path)],directory,'rule'))
        wait_socket(rule_socket,rule,log)
        proxy, log = stack.enter_context(process([str(binaries/'astra-proxy'),str(proxy_path)],directory,'proxy'))
        wait_port(port,proxy,log)
        with socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as udp, socket.socket(socket.AF_INET,socket.SOCK_DGRAM) as intruder:
            udp.bind(('127.0.0.1',0)); udp.settimeout(.2)
            intruder.bind(('127.0.0.1',0)); intruder.settimeout(.2)
            control, relay = association(port,udp.getsockname()[1])
            with control:
                intruder.sendto(packet(ports[0]),relay); silent(intruder)
                # Fragmented and truncated requests must not kill a healthy association.
                udp.sendto(b'\x00\x00\x01'+packet(ports[0])[3:],relay); silent(udp)
                udp.sendto(b'\x00\x00\x00\x03\xff',relay); silent(udp)
                for length in (0,1,1200,8192):
                    payload=bytes(i%251 for i in range(length))
                    udp.sendto(packet(ports[0],payload),relay)
                    assert udp.recv(65536)==packet(ports[0],payload)
                tcp, status = socks(port, ports[0])
                tcp.close()
                assert status == 2, 'UDP route cache incorrectly allowed TCP'
                udp.sendto(packet(ports[1]),relay); silent(udp)
                # One association cannot bypass the global flow limit.
                other, other_relay=association(port,intruder.getsockname()[1])
                with other:
                    intruder.sendto(packet(ports[1]),other_relay); silent(intruder)
                udp.sendto(packet(ports[0]),relay); assert udp.recv(65536)==packet(ports[0])
            time.sleep(.1)
            udp.sendto(packet(ports[0]),relay); silent(udp)
            replacement, relay=association(port,udp.getsockname()[1])
            with replacement:
                udp.sendto(packet(ports[1]),relay); assert udp.recv(65536)==packet(ports[1])
                replacement.settimeout(4)
                assert replacement.recv(1)==b'', 'idle association did not expire'
            print('PASS UDP direct: 0..8192 bytes, network rules/cache separation, endpoint isolation, fragment rejection, flow limits, control-close cleanup, idle expiry',flush=True)
        for echo in echoes: echo.shutdown()

if __name__=='__main__': main()
