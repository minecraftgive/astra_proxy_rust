#!/usr/bin/env python3
"""Test-only RFC 5780 dual-IP/dual-port server; not a product listener.

Used by test_nat_namespaces.py inside an isolated Linux network namespace.
No DNS lookup, authentication, deployment, or daemon integration is performed.
"""
import argparse
import ipaddress
import selectors
import socket
import struct

COOKIE = 0x2112A442


def attribute(kind, address, transaction, xor=False):
    ip = ipaddress.ip_address(address[0])
    mask = struct.pack("!I", COOKIE) + transaction
    encoded = bytes(byte ^ (mask[index] if xor else 0) for index, byte in enumerate(ip.packed))
    value = struct.pack("!BBH", 0, 1 if ip.version == 4 else 2,
                        address[1] ^ ((COOKIE >> 16) if xor else 0)) + encoded
    return struct.pack("!HH", kind, len(value)) + value


def response(transaction, peer, source, other):
    body = attribute(0x0020, peer, transaction, True)
    body += attribute(0x802B, source, transaction)
    body += attribute(0x802C, other, transaction)
    return struct.pack("!HHI", 0x0101, len(body), COOKIE) + transaction + body


def serve(address, alternate, port, other_port):
    if address == alternate or port == other_port:
        raise ValueError("Two distinct addresses and ports are required")
    endpoints = [(address, port), (address, other_port), (alternate, port), (alternate, other_port)]
    selector = selectors.DefaultSelector()
    sockets = []
    try:
        for index, endpoint in enumerate(endpoints):
            family = socket.AF_INET6 if ipaddress.ip_address(endpoint[0]).version == 6 else socket.AF_INET
            udp = socket.socket(family, socket.SOCK_DGRAM)
            udp.bind(endpoint)
            sockets.append(udp)
            selector.register(udp, selectors.EVENT_READ, index)
        print("READY", flush=True)
        while True:
            for key, _ in selector.select():
                packet, peer = key.fileobj.recvfrom(4097)
                if len(packet) < 20 or len(packet) > 4096:
                    continue
                kind, length, cookie = struct.unpack("!HHI", packet[:8])
                if kind != 1 or cookie != COOKIE or length % 4 or length + 20 != len(packet):
                    continue
                change, cursor, valid = 0, 20, True
                while cursor < len(packet):
                    attr, size = struct.unpack("!HH", packet[cursor:cursor + 4])
                    end = cursor + 4 + size
                    if end > len(packet):
                        valid = False
                        break
                    if attr == 3:
                        if size != 4:
                            valid = False
                            break
                        change = struct.unpack("!I", packet[cursor + 4:end])[0]
                    cursor = end + (-size % 4)
                if not valid or change not in (0, 2, 4, 6):
                    continue
                index = key.data ^ ({0: 0, 2: 1, 4: 2, 6: 3}[change])
                wire = response(packet[8:20], peer, endpoints[index], endpoints[index ^ 3])
                sockets[index].sendto(wire, peer)
    finally:
        selector.close()
        for udp in sockets:
            udp.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--address", required=True)
    parser.add_argument("--alternate", required=True)
    parser.add_argument("--port", type=int, default=3478)
    parser.add_argument("--other-port", type=int, default=3479)
    args = parser.parse_args()
    serve(args.address, args.alternate, args.port, args.other_port)


if __name__ == "__main__":
    main()
