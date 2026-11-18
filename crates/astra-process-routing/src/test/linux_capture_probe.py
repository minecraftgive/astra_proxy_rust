"""Packet assertions for linux_capture_test.rs, inside its private network namespace."""
import errno
import os
import signal
import socket
import struct
import subprocess
import sys
import threading
from contextlib import ExitStack
from pathlib import Path


def checked(*args, timeout=10, **kwargs):
    return subprocess.run(args, check=True, timeout=timeout, **kwargs)


def echo(listener, label, tcp):
    while True:
        try:
            if tcp:
                with listener.accept()[0] as stream:
                    data = stream.recv(100)
                    endpoint = stream.getsockname()
                    reply = f"{label} {endpoint[0]} {endpoint[1]} ".encode()
                    stream.sendall(reply + data)
            else:
                data, control, _, peer = listener.recvmsg(100, 256)
                endpoint = next((payload for level, kind, payload in control
                                 if (level, kind) in ((socket.IPPROTO_IP, 20), (socket.IPPROTO_IPV6, 74))), None)
                assert endpoint
                port = struct.unpack_from("!H", endpoint, 2)[0]
                raw = endpoint[4:8] if listener.family == socket.AF_INET else endpoint[8:24]
                host = socket.inet_ntop(listener.family, raw)
                with socket.socket(listener.family, socket.SOCK_DGRAM) as reply:
                    reply.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                    reply.bind((host, port))
                    reply.sendto(f"{label} {host} {port} ".encode() + data, peer)
        except OSError:
            return


def serve(stack, family, address, port, label):
    for protocol in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
        sock = stack.enter_context(socket.socket(family, protocol))
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        level, transparent, original = (socket.IPPROTO_IP, 19, 20) if family == socket.AF_INET else (socket.IPPROTO_IPV6, 75, 74)
        if family == socket.AF_INET6:
            sock.setsockopt(level, socket.IPV6_V6ONLY, 1)
        if label == "captured":
            sock.setsockopt(level, transparent, 1)
        if protocol == socket.SOCK_DGRAM:
            sock.setsockopt(level, original, 1)
        sock.bind((address, port))
        if protocol == socket.SOCK_STREAM:
            sock.listen()
        threading.Thread(target=echo, args=(sock, label, protocol == socket.SOCK_STREAM), daemon=True).start()


def probe(expected):
    for family, host in ((socket.AF_INET, "127.0.0.2"), (socket.AF_INET6, "::1")):
        for kind in ("tcp", "connected_udp", "udp"):
            with socket.socket(family, socket.SOCK_STREAM if kind == "tcp" else socket.SOCK_DGRAM) as sock:
                sock.settimeout(1)
                for port in ([31212, 31213] if kind == "udp" else [31212]):
                    endpoint = (host, port)
                    data = os.urandom(12)
                    try:
                        if kind == "udp":
                            sock.sendto(data, endpoint)
                        else:
                            sock.connect(endpoint)
                            sock.sendall(data)
                        response = sock.recv(100)
                    except OSError as exc:
                        if expected == "denied" and exc.errno == errno.EPERM:
                            continue
                        if expected == "dropped" and isinstance(exc, TimeoutError):
                            continue
                        raise
                    assert response == f"{expected} {host} {port} ".encode() + data, (kind, endpoint, expected, response)


def run(phase, cgroup, target, control):
    assert os.readlink("/proc/self/ns/net") != os.readlink("/proc/1/ns/net")
    checked("ip", "link", "set", "lo", "up")

    def client(executable, expected):
        # A separate interpreter of the same basename exercises exact path matching.
        checked(executable, __file__, "client", expected,
                preexec_fn=lambda: Path(cgroup, "cgroup.procs").write_text(str(os.getpid())))

    with ExitStack() as stack:
        for family, address in ((socket.AF_INET, "127.0.0.2"), (socket.AF_INET6, "::1")):
            for port in (31212, 31213):
                serve(stack, family, address, port, "normal")
        if phase == "transparent":
            rules = """table inet astra_probe {
                chain output { type route hook output priority mangle; policy accept;
                    socket mark 77 meta mark set 177
                }
                chain ingress { type filter hook prerouting priority mangle; policy accept;
                    meta mark 177 meta l4proto { tcp, udp } tproxy to :31211 accept
                    meta mark 177 drop
                }
            }"""
            checked("nft", "-f", "-", input=rules, text=True)
            with ExitStack() as capture:
                serve(capture, socket.AF_INET, "0.0.0.0", 31211, "captured")
                serve(capture, socket.AF_INET6, "::", 31211, "captured")
                client(target, "captured")
                client(control, "normal")
            # Explicit drop if TPROXY cannot find its listener.
            # Daemon threads may still hold blocked recv/accept references, so test
            # failure with an unused port instead of relying on cross-thread close.
            checked("nft", "-f", "-", input="flush chain inet astra_probe ingress\n"
                    "add rule inet astra_probe ingress meta mark 177 meta l4proto {tcp,udp} tproxy to :31214 accept\n"
                    "add rule inet astra_probe ingress meta mark 177 drop\n", text=True)
            client(target, "dropped")
            client(control, "normal")
            checked("nft", "delete", "table", "inet", "astra_probe")
        else:
            client(target, "denied" if phase == "block" else "normal")
            client(control, "normal")
            checked("unshare", "--mount", "--propagation", "private", target, __file__, "client",
                    "denied" if phase == "block" else "normal",
                    preexec_fn=lambda: Path(cgroup, "cgroup.procs").write_text(str(os.getpid())))


def hold(cgroup):
    signal.alarm(10)
    with ExitStack() as stack:
        server = stack.enter_context(socket.socket())
        server.bind(("127.0.0.1", 0))
        server.listen()
        # Leave the test harness outside this cgroup. Only this executable
        # and its existing/new client sockets participate in the update.
        Path(cgroup, "cgroup.procs").write_text(str(os.getpid()))
        tcp = stack.enter_context(socket.create_connection(server.getsockname(), 2))
        peer = stack.enter_context(server.accept()[0])
        udp = stack.enter_context(socket.socket(type=socket.SOCK_DGRAM))
        udp.sendto(b"before", ("127.0.0.1", 31051))
        assert all(s.getsockopt(socket.SOL_SOCKET, socket.SO_MARK) == 77 for s in (tcp, udp))
        print("ready", flush=True)
        assert sys.stdin.readline().strip() == "updated"
        tcp.sendall(b"after")
        assert peer.recv(8) == b"after"
        udp.sendto(b"after", ("127.0.0.1", 31052))
        assert all(s.getsockopt(socket.SOL_SOCKET, socket.SO_MARK) == 77 for s in (tcp, udp))
        for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
            with socket.socket(type=kind) as fresh:
                try:
                    if kind == socket.SOCK_STREAM:
                        fresh.connect(server.getsockname())
                    else:
                        fresh.sendto(b"denied", ("127.0.0.1", 31051))
                except PermissionError as exc:
                    assert exc.errno == errno.EPERM
                else:
                    raise AssertionError("new socket escaped updated BLOCK rule")


if __name__ == "__main__":
    if sys.argv[1] == "client":
        probe(sys.argv[2])
    elif sys.argv[1] == "hold":
        hold(sys.argv[2])
    else:
        run(*sys.argv[1:])
