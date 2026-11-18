"""Native Linux integration. The origin observes which of two veth pairs carried each request."""
import os
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from contextlib import ExitStack
from pathlib import Path

from linux_capture_probe import checked

FAMILIES = ((socket.AF_INET, "198.51.100.42"), (socket.AF_INET6, "2001:db8:ff::42"))
PORTS = [31441, 31442]


def serve(sock, journal):
    while True:
        if sock.type == socket.SOCK_STREAM:
            stream, peer = sock.accept()
            with stream:
                with journal.open("a") as log:
                    print("tcp", peer[0], file=log)
                stream.sendall(peer[0].encode() + b" " + stream.recv(200))
        else:
            data, peer = sock.recvfrom(200)
            with journal.open("a") as log:
                print("udp", peer[0], file=log)
            sock.sendto(peer[0].encode() + b" " + data, peer)


def origin(journal):
    with ExitStack() as stack:
        for family, host in FAMILIES:
            for port in PORTS:
                for kind in (socket.SOCK_DGRAM, socket.SOCK_STREAM):
                    sock = stack.enter_context(socket.socket(family, kind))
                    sock.bind((host, port))
                    if kind == socket.SOCK_STREAM:
                        sock.listen()
                    threading.Thread(target=serve, args=(sock, journal), daemon=True).start()
        print("ready", flush=True)
        signal.pause()


def probe(expected):
    for family, host in FAMILIES:
        for kind in ("tcp", "connected_udp", "sendto"):
            with socket.socket(family, socket.SOCK_STREAM if kind == "tcp" else socket.SOCK_DGRAM) as sock:
                sock.settimeout(1.5)
                for port in PORTS if kind == "sendto" else PORTS[:1]:
                    data = os.urandom(16)
                    try:
                        if kind == "sendto":
                            sock.sendto(data, (host, port))
                        else:
                            sock.connect((host, port))
                            sock.sendall(data)
                        reply = sock.recv(200)
                    except OSError as exc:
                        if expected != "blocked":
                            raise RuntimeError((kind, host, port, expected)) from exc
                        if isinstance(exc, (TimeoutError, ConnectionResetError, ConnectionRefusedError)):
                            continue
                        raise
                    if expected == "blocked" and kind == "tcp" and not reply:
                        continue
                    peer = f"192.0.{expected}.1" if family == socket.AF_INET else f"2001:db8:{expected}::1"
                    assert reply == peer.encode() + b" " + data, (kind, host, expected, reply)


def stop(child):
    if child.poll() is None:
        child.kill()
    child.wait(timeout=5)


def native(cgroup, target, control):
    assert os.readlink("/proc/self/ns/net") != os.readlink("/proc/1/ns/net")
    with tempfile.TemporaryDirectory() as temp, ExitStack() as stack:
        root = Path(temp)
        remote = subprocess.Popen(["unshare", "--net", "sleep", "120"])
        stack.callback(stop, remote)
        until = time.monotonic() + 5
        while os.readlink(f"/proc/{remote.pid}/ns/net") == os.readlink("/proc/self/ns/net"):
            assert time.monotonic() < until and remote.poll() is None
            time.sleep(.01)
        ns = ["nsenter", "-n", "-t", str(remote.pid)]
        checked(*ns, "ip", "link", "set", "lo", "up")
        for n in (1, 2):
            link, peer = f"test{n}", f"peer{n}"
            checked("ip", "link", "add", link, "type", "veth", "peer", "name", peer, "netns", str(remote.pid))
            checked("ip", "link", "set", link, "up")
            checked(*ns, "ip", "link", "set", peer, "up")
            for ipv6 in (False, True):
                left = f"2001:db8:{n}::1/64" if ipv6 else f"192.0.{n}.1/24"
                right = f"2001:db8:{n}::2/64" if ipv6 else f"192.0.{n}.2/24"
                flags = ["nodad"] if ipv6 else []
                checked("ip", "addr", "add", left, "dev", link, *flags)
                checked(*ns, "ip", "addr", "add", right, "dev", peer, *flags)
                checked("ip", "-6" if ipv6 else "-4", "route", "add", "default", "via",
                        right.split("/")[0], "dev", link, "metric", str(10 * n))
            for prefix in ([], ns):
                checked(*prefix, "sysctl", "-q", "-w", "net.ipv4.conf.all.rp_filter=0")
            checked("sysctl", "-q", "-w", f"net.ipv4.conf.{link}.rp_filter=0")
            checked(*ns, "sysctl", "-q", "-w", f"net.ipv4.conf.{peer}.rp_filter=0")
        for family, host in FAMILIES:
            checked(*ns, "ip", "addr", "add", host + ("/32" if family == socket.AF_INET else "/128"), "dev", "lo")
            ip = ["ip", "-4" if family == socket.AF_INET else "-6"]
            checked(*ip, "rule", "add", "pref", "100", "fwmark", "177", "table", "177")
            source = "192.0.1.1" if family == socket.AF_INET else "2001:db8:1::1"
            checked(*ip, "route", "add", "local", "default", "dev", "lo", "src", source, "table", "177")

        journal = root / "received"
        journal.touch()
        server = subprocess.Popen([*ns, sys.executable, __file__, "origin", journal], stdout=subprocess.PIPE)
        stack.callback(stop, server)
        assert select.select([server.stdout], [], [], 5)[0] and server.stdout.readline() == b"ready\n"
        checked("nft", "-f", "-", text=True, input="""table inet astra_core_test {
            chain output { type route hook output priority mangle; policy accept;
                socket mark 77 meta mark set 177
            }
            chain ingress { type filter hook prerouting priority mangle; policy accept;
                meta mark 177 meta l4proto {tcp, udp} tproxy to :31440 accept
                meta mark 177 drop
            }
        }""")

        def client(path, expected):
            before = journal.stat().st_size
            checked(path, __file__, "client", expected, timeout=20,
                    preexec_fn=lambda: Path(cgroup, "cgroup.procs").write_text(str(os.getpid())))
            # A failed round trip alone could hide a one-way datagram leak.
            # Check the origin even when the selected egress is unavailable.
            if expected == "blocked":
                time.sleep(.05)
                assert journal.stat().st_size == before, "origin received traffic while egress was down"
            else:
                with journal.open() as log:
                    log.seek(before)
                    events = [line.split() for line in log]
                families = {f"192.0.{expected}.1", f"2001:db8:{expected}::1"}
                assert len(events) == 8 and {addr for _, addr in events} == families, events

        def cores(stack):
            for n, bind in enumerate(["0.0.0.0", "[::]"]):
                profile = root / f"p{n}.toml"
                profile.write_text(f'''standalone = true
standalone_outbound = "bound"
[tproxy]
listen = "{bind}:31440"
[proxy_groups.bound]
members = ["DIRECT"]
interface = "test2"
''')
                ctl = root / f"p{n}.sock"
                env = {**os.environ, "ASTRA_RUNTIME_DIR": temp, "ASTRA_CONTROL_SOCKET": str(ctl)}
                child = subprocess.Popen([os.environ["ASTRA_CAPTURE_CORE"], profile], env=env)
                stack.callback(stop, child)
                deadline = time.monotonic() + 10
                while not ctl.exists():
                    assert child.poll() is None and time.monotonic() < deadline
                    time.sleep(.02)
                # A successful TCP connect confirms the data listener has been committed.
                address = ("127.0.0.1" if n == 0 else "::1", 31440)
                while True:
                    try:
                        with socket.create_connection(address, .2):
                            break
                    except OSError:
                        assert child.poll() is None and time.monotonic() < deadline
                        time.sleep(.02)

        with ExitStack() as core_stack:
            cores(core_stack)
            client(target, "2")
            client(control, "1")
            checked("ip", "link", "set", "test2", "down")
            client(target, "blocked")
            client(control, "1")
            checked("ip", "link", "set", "test2", "up")
        client(target, "blocked")
        client(control, "1")
        checked("nft", "delete", "table", "inet", "astra_core_test")


if __name__ == "__main__":
    if sys.argv[1] == "origin":
        origin(Path(sys.argv[2]))
    elif sys.argv[1] == "client":
        probe(sys.argv[2])
    else:
        native(*sys.argv[1:])
