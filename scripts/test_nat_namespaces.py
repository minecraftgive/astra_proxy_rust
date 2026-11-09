#!/usr/bin/env python3
"""Run the Rust NAT engine across nine isolated Linux kernel NAT combinations.

Requires Linux root (network namespaces, veth, iptables + xt_recent), and the
astra-proxy lib test executable built by cargo test --no-run. Nothing is added
to host routing/firewall tables; all addresses and policy live in three unique
namespaces, which are deleted even when a test fails. Not shipped as a service.
"""
import argparse
import itertools
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import sys
import uuid

BEHAVIORS = ("endpoint_independent", "address_dependent", "address_and_port_dependent")


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def case(binary, mapping, filtering):
    prefix = "an" + uuid.uuid4().hex[:7]
    client, router, server = (prefix + suffix for suffix in ("c", "r", "s"))
    namespaces = []
    service = None

    def ns(name, *args, **kwargs):
        return run("ip", "netns", "exec", name, *args, **kwargs)

    def firewall(*args):
        ns(router, "iptables", "-w", *args)

    try:
        for name in (client, router, server):
            run("ip", "netns", "add", name)
            namespaces.append(name)
            ns(name, "ip", "link", "set", "lo", "up")
        for left, right, left_ns, right_ns in ((prefix + "a", prefix + "b", client, router), (prefix + "d", prefix + "e", router, server)):
            run("ip", "link", "add", left, "type", "veth", "peer", "name", right)
            run("ip", "link", "set", left, "netns", left_ns)
            run("ip", "link", "set", right, "netns", right_ns)
        for name, device, address in ((client, prefix + "a", "10.77.0.2/24"), (router, prefix + "b", "10.77.0.1/24"),
                                      (router, prefix + "d", "198.18.0.1/24"), (server, prefix + "e", "198.18.0.2/24")):
            ns(name, "ip", "address", "add", address, "dev", device)
            ns(name, "ip", "link", "set", device, "up")
        ns(server, "ip", "address", "add", "198.18.0.3/24", "dev", prefix + "e")
        ns(client, "ip", "route", "add", "default", "via", "10.77.0.1")
        ns(router, "sysctl", "-qw", "net.ipv4.ip_forward=1")
        firewall("-P", "FORWARD", "DROP")
        endpoints = list(itertools.product(("198.18.0.2", "198.18.0.3"), (3478, 3479)))
        # Test-only sockets reserve three fresh ports: mapping, capabilities,
        # filtering. Stable SNAT rules model each destination dependence class.
        for source_port in (35000, 35001, 35002):
            external_ports = set()
            for index, (destination, port) in enumerate(endpoints):
                bucket = 0 if mapping == BEHAVIORS[0] else index // 2 if mapping == BEHAVIORS[1] else index
                external = source_port + 10000 + bucket * 100
                external_ports.add(external)
                # A changed-source reply to another mapping's public port can
                # otherwise collide with an existing conntrack reverse tuple
                # after DNAT. Separate translation buckets into zones so the
                # kernel models independent mappings rather than rejecting
                # that packet before our explicit filtering policy runs.
                firewall("-t", "raw", "-A", "PREROUTING", "-p", "udp", "-s", "10.77.0.2", "--sport", str(source_port),
                         "-d", destination, "--dport", str(port), "-j", "CT", "--zone", str(100 + bucket))
                firewall("-t", "nat", "-A", "POSTROUTING", "-p", "udp", "-s", "10.77.0.2", "--sport", str(source_port),
                         "-d", destination, "--dport", str(port), "-j", "SNAT", "--to-source", f"198.18.0.1:{external}")
                filter_bucket = 0 if filtering == BEHAVIORS[0] else index // 2 if filtering == BEHAVIORS[1] else index
                name = f"p{source_port}b{filter_bucket}"
                firewall("-A", "FORWARD", "-p", "udp", "-s", "10.77.0.2", "--sport", str(source_port), "-d", destination,
                         "--dport", str(port), "-m", "recent", "--name", name, "--rsource", "--set", "-j", "ACCEPT")
                firewall("-A", "FORWARD", "-p", "udp", "-s", destination, "--sport", str(port), "-d", "10.77.0.2",
                         "--dport", str(source_port), "-m", "recent", "--name", name, "--rdest", "--rcheck", "-j", "ACCEPT")
            for external in external_ports:
                bucket = (external - source_port - 10000) // 100
                firewall("-t", "raw", "-A", "PREROUTING", "-p", "udp", "-d", "198.18.0.1", "--dport", str(external),
                         "-j", "CT", "--zone", str(100 + bucket))
                firewall("-t", "nat", "-A", "PREROUTING", "-p", "udp", "-d", "198.18.0.1", "--dport", str(external),
                         "-j", "DNAT", "--to-destination", f"10.77.0.2:{source_port}")
        service_path = Path(__file__).with_name("nat_test_service.py")
        service = subprocess.Popen(["ip", "netns", "exec", server, sys.executable, str(service_path), "--address", "198.18.0.2", "--alternate", "198.18.0.3"], stdout=subprocess.PIPE, text=True)
        ready = selectors.DefaultSelector()
        ready.register(service.stdout, selectors.EVENT_READ)
        if not ready.select(5) or service.stdout.readline().strip() != "READY":
            raise RuntimeError("Local STUN service failed to become ready")
        ready.close()
        env = dict(os.environ, ASTRA_NAT_FIXTURE_MAPPING=mapping, ASTRA_NAT_FIXTURE_FILTERING=filtering)
        ns(client, str(binary), "network_namespace_matrix_case", "--ignored", "--nocapture", env=env, timeout=45)
        print(f"PASS {mapping} / {filtering}", flush=True)
    finally:
        if service:
            service.terminate()
            try:
                service.wait(timeout=5)
            except subprocess.TimeoutExpired:
                service.kill()
                service.wait()
        for name in reversed(namespaces):
            subprocess.run(["ip", "netns", "delete", name], check=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test-binary", required=True, type=Path)
    args = parser.parse_args()
    if sys.platform != "linux" or os.geteuid() != 0:
        parser.error("Requires Linux root; use the portable Rust fixture on other hosts")
    if not args.test_binary.is_file() or not all(shutil.which(tool) for tool in ("ip", "iptables")):
        parser.error("Provide a compiled lib test executable and install iproute2/iptables")
    for mapping, filtering in itertools.product(BEHAVIORS, repeat=2):
        case(args.test_binary.resolve(), mapping, filtering)


if __name__ == "__main__":
    main()
