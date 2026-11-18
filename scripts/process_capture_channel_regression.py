#!/usr/bin/env python3
"""Run the macOS provider's data-channel code against an actual Astra core, without OS capture."""
import argparse
import contextlib
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import threading
import time

from astra_tools.fixtures.meter import Meter
from astra_tools.fixtures.protocols import fixture, write_json
from astra_tools.net import Echo, EchoServer, UdpEcho, UdpEchoServer, free_port, socks
from astra_tools.processes import process, wait_port, wait_socket
from astra_tools.workspace import ROOT, reference_engine


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(dir="/tmp") as tmp, contextlib.ExitStack() as stack:
        root = Path(tmp)
        source = ROOT / "crates/astra-process-routing"
        probe = root / "probe"
        subprocess.run(["xcrun", "swiftc", "-target", f"{platform.machine()}-apple-macos15",
                        "-parse-as-library", str(source / "native/macos/Policy.swift"), str(source / "native/macos/Channel.swift"),
                        str(source / "src/test/MacChannelProbe.swift"), "-o", str(probe)], check=True)
        port = free_port()
        _, config, _ = fixture("socks-plain", port, None, None)
        child, logfile = stack.enter_context(process([str(reference_engine("xray")), "run", "-config",
                    str(write_json(root / "xray.json", config))], root, "xray"))
        wait_port(port, child, logfile)
        meter = stack.enter_context(EchoServer(("127.0.0.1", 0), Meter))
        meter.destination, meter.connections, meter.lock = port, 0, threading.Lock()
        tcp = stack.enter_context(EchoServer(("127.0.0.1", 0), Echo))
        udp = stack.enter_context(UdpEchoServer(("127.0.0.1", 0), UdpEcho))
        for server in [meter, tcp, udp]:
            threading.Thread(target=server.serve_forever, daemon=True).start()
            stack.callback(server.shutdown)
        socks_port = free_port()
        config = root / "proxy.toml"
        config.write_text(f'''standalone = true
standalone_outbound = "offline"
socks5_listen = "127.0.0.1:{socks_port}"
[[outbounds]]
kind = "socks5"
tag = "offline"
server = "127.0.0.1:{free_port()}"
[[outbounds]]
kind = "socks5"
tag = "observed"
server = "127.0.0.1:{meter.server_address[1]}"
[proxy_groups.selected]
members = ["observed"]
''')
        endpoint = root / "capture"
        core = subprocess.Popen([args.bin_dir.resolve() / "astra-proxy", config],
                env={**os.environ, "ASTRA_CONTROL_SOCKET": str(root / "ctl"), "ASTRA_PROCESS_INGRESS": str(endpoint),
                     "ASTRA_RUNTIME_DIR": str(root / "runtime")})
        def stop():
            if core.poll() is None:
                core.terminate()
            core.wait(timeout=5)
        stack.callback(stop)
        deadline = time.monotonic() + 5
        while not endpoint.exists():
            assert core.poll() is None and time.monotonic() < deadline
            time.sleep(.02)
        for network, listener in [("Tcp", tcp), ("Udp", udp)]:
            for kind, value, success in [("direct", "", True), ("proxy", "selected", True),
                                         ("interface", "lo0", True), ("interface", "astra-missing0", False),
                                         ("proxy", "unknown", False), ("block", "", False)]:
                before = meter.connections
                result = subprocess.run([probe, endpoint, network, "127.0.0.1", str(listener.server_address[1]),
                                          kind, value], timeout=12)
                assert result.returncode == (0 if success else 2), (network, kind, result.returncode)
                assert meter.connections - before == int(kind == "proxy" and success)
        # Switch the same core from standalone to a live Rule Core that blocks
        # the destination. Explicit application bindings still own their flow.
        rules = root / "rules.toml"
        rule_socket = root / "rules.sock"
        rules.write_text(f'socket="{rule_socket}"\ndirect_local=false\ndefault_action="block"\n')
        child, logfile = stack.enter_context(process([args.bin_dir.resolve() / "astra-rule", rules], root, "rules"))
        wait_socket(rule_socket, child, logfile)
        config.write_text(config.read_text().replace('standalone = true\nstandalone_outbound = "offline"',
                                                     f'rule_socket = "{rule_socket}"'))
        subprocess.run([args.bin_dir.resolve() / "astra-proxy", "--reload", root / "ctl"],
                       check=True, capture_output=True, timeout=10)
        denied, status = socks(socks_port, tcp.server_address[1])
        with denied:
            assert status == 2, status
        for network, listener in [("Tcp", tcp), ("Udp", udp)]:
            before = meter.connections
            subprocess.run([probe, endpoint, network, "127.0.0.1", str(listener.server_address[1]),
                            "proxy", "selected"], check=True, timeout=12)
            assert meter.connections == before + 1
        print("PASS actual provider channel, TCP, UDP including zero length, DIRECT, selected group, BLOCK and missing node; explicit group overrides live BLOCK policy")


if __name__ == "__main__":
    main()
