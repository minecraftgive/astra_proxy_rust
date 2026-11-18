#!/usr/bin/env python3
"""Exercise a signed, approved macOS provider using two remote dual-stack echo ports."""
import argparse
from contextlib import ExitStack
import ipaddress
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time

from astra_tools.net import free_port
from astra_tools.workspace import ROOT


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True, type=Path, help="signed test .app/Contents/MacOS/AstraProcessRouting")
    parser.add_argument("--core", required=True, type=Path)
    parser.add_argument("--ipv4", required=True, type=ipaddress.IPv4Address)
    parser.add_argument("--ipv6", required=True, type=ipaddress.IPv6Address)
    parser.add_argument("--ports", nargs=2, type=int, required=True, help="two remote ports with TCP and UDP echo")
    args = parser.parse_args()
    if (len(set(args.ports)) != 2 or not all(0 < p < 65536 for p in args.ports)
            or any(ip.is_loopback or ip.is_unspecified for ip in (args.ipv4, args.ipv6))):
        parser.error("supply reachable remote IPv4/IPv6 addresses and two distinct echo ports")
    if subprocess.check_output([args.host, "status"], text=True).strip() != "absent":
        parser.error("this test requires its own bundle ID with no saved proxy configuration")
    with tempfile.TemporaryDirectory(dir="/tmp") as temp, ExitStack() as cleanup:
        root = Path(temp)
        target, control = (root / name / "client" for name in ("bound", "control"))
        target.parent.mkdir()
        control.parent.mkdir()
        subprocess.run(["rustc", "--edition", "2021", ROOT / "crates/astra-process-routing/src/test/native_echo_client.rs",
                        "-o", target], check=True)
        shutil.copy2(target, control)
        endpoint = root / "capture"
        profile = root / "proxy.toml"
        profile.write_text(f'standalone = true\nstandalone_outbound = "offline"\n'
                           f'socks5_listen = "127.0.0.1:{free_port()}"\n[[outbounds]]\nkind = "socks5"\n'
                           f'tag = "offline"\nserver = "127.0.0.1:{free_port()}"\n')
        core = subprocess.Popen([args.core, profile], env={**os.environ, "ASTRA_RUNTIME_DIR": temp,
                                "ASTRA_CONTROL_SOCKET": str(root / "control.sock"), "ASTRA_PROCESS_INGRESS": str(endpoint)})
        def stop():
            if core.poll() is None:
                core.kill()
            core.wait(timeout=5)
        cleanup.callback(stop)
        deadline = time.monotonic() + 10
        while not endpoint.exists():
            assert core.poll() is None and time.monotonic() < deadline
            time.sleep(.05)

        def probe(exe, expected):
            for host in (str(args.ipv4), f"[{args.ipv6}]"):
                subprocess.run([exe, expected, *(f"{host}:{p}" for p in args.ports)], check=True, timeout=20)

        def apply(egress):
            policy = root / "policy.json"
            policy.write_text(json.dumps([dict(path=str(target.resolve()), socket=str(endpoint), egress=egress)]))
            subprocess.run([args.host, "apply", policy], check=True, timeout=45)

        probe(target, "allow")
        probe(control, "allow")
        # The test bundle has its own extension and no prior configuration.
        cleanup.callback(subprocess.run, [args.host, "clear"], check=True, timeout=45)
        apply({"kind": "block"})
        probe(target, "block")
        probe(control, "allow")
        apply({"kind": "direct"})
        probe(target, "allow")
        apply({"kind": "proxy", "tag": "offline"})
        probe(target, "block")
        apply({"kind": "direct"})
        stop()
        probe(target, "block")
        probe(control, "allow")
    print("PASS signed system capture, exact path, UDP peers and fail-closed core loss")


if __name__ == "__main__":
    main()
