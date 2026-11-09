#!/usr/bin/env python3
"""Opt-in Windows TUN diagnostics fixture; only two owned host routes, no default route.

Run from an authorized elevated test process. All product state is confined to
--output-dir; --rpc-helper is a task-local ManagerClient JSON bridge. The
production endpoint is read only and is checked again after cleanup.
"""
import argparse
import ctypes
import ipaddress
import json
import os
from pathlib import Path
import subprocess
import time
import uuid

from astra_tools.net import free_port
from astra_tools.windows import base, powershell, upstream

TARGETS = ("198.19.253.2", "fd00:198:19:253::2")
PREFIXES = (TARGETS[0] + "/32", TARGETS[1] + "/128")
ADDRESSES = ("198.18.253.1", "fd00:198:18:253::1")
AGENT = "windows-tun-diagnostics-test"


def ps_json(script):
    return json.loads(powershell("$value = " + script + "; ConvertTo-Json -InputObject $value -Depth 8 -Compress"))


def rpc(args, endpoint, body, agent=None):
    request = {"version": 2, "id": str(uuid.uuid4()), "agent_id": agent, "body": body}
    result = subprocess.run(
        [str(args.rpc_helper), str(endpoint)], input=json.dumps(request),
        text=True, encoding="utf-8", capture_output=True, timeout=15, check=False,
    )
    if result.returncode:
        raise RuntimeError(f"RPC helper failed ({result.returncode}): {result.stderr.strip()}")
    reply = json.loads(result.stdout)["result"]
    if "Err" in reply:
        raise RuntimeError(json.dumps(reply["Err"]))
    return reply["Ok"]


def query(args, endpoint, kind, data=None, agent=None):
    body = {"type": kind}
    if data is not None:
        body["data"] = data
    return rpc(args, endpoint, {"type": "query", "data": body}, agent)["data"]


def execute(args, endpoint, kind, data=None):
    command = {"type": kind}
    if data is not None:
        command["data"] = data
    return rpc(args, endpoint, {"type": "execute", "data": {
        "operation_id": "windows-tun-" + str(uuid.uuid4()), "command": command,
    }}, AGENT)


def production(args):
    runtime = query(args, args.production_endpoint, "runtime", {"instance": args.production_instance})["snapshot"]
    return {key: runtime[key] for key in ("pid", "tun", "mode", "system_proxy")}


def report(args, endpoint, operation):
    deadline = time.monotonic() + 40
    cursor, events = None, []
    while time.monotonic() < deadline:
        page = query(args, endpoint, "network_report", {"operation_id": operation, "cursor": cursor}, AGENT)
        events.extend(item["data"] for item in page["items"])
        cursor = page["cursor"]
        if not page["has_more"] and page["state"] not in ("queued", "running"):
            return {"state": page["state"], "message": page["message"], "events": events}
        time.sleep(.1)
    raise TimeoutError("bounded diagnostic did not reach a terminal report")


def probe(args, endpoint, instance, probe, family="ipv4", egress=None):
    result = execute(args, endpoint, "network_diagnostics", {
        "instance": instance,
        "request": {"egress": egress or {"kind": "system"}, "probe": probe,
                    "family": family, "resolver": None, "count": 1,
                    "interval_ms": "0", "timeout_ms": "1000"},
    })
    assert result["type"] == "task", result
    return report(args, endpoint, result["data"]["id"])


def route_snapshot():
    return ps_json("@(Get-NetRoute -ErrorAction Stop | Select-Object DestinationPrefix,InterfaceIndex,NextHop,RouteMetric | Sort-Object InterfaceIndex,DestinationPrefix,NextHop)")


def owned_routes():
    prefixes = "','".join(PREFIXES)
    return ps_json("@(Get-NetRoute -ErrorAction Stop | Where-Object {$_.DestinationPrefix -in @('" + prefixes + "')} | Select-Object DestinationPrefix,InterfaceIndex,NextHop)")


def run(args):
    assert os.name == "nt" and ctypes.windll.shell32.IsUserAnAdmin(), "authorized elevated test process required"
    assert args.physical_interface.isdigit(), "explicit physical interface index required"
    assert args.wintun.is_file() and args.rpc_helper.is_file()
    args.output_dir.mkdir(parents=True, exist_ok=False)
    name = "AstraDiagTest" + str(os.getpid())
    assert not owned_routes(), "test host routes already exist"
    addresses = "','".join(ADDRESSES + TARGETS)
    assert not ps_json("@(Get-NetIPAddress -ErrorAction Stop | Where-Object {$_.IPAddress -in @('" + addresses + "')} | Select-Object IPAddress)"), "test addresses already exist"
    assert not ps_json(f"@(Get-NetAdapter -Name '{name}' -ErrorAction SilentlyContinue | Select-Object ifIndex)"), "test adapter already exists"
    before = production(args)
    routes_before = route_snapshot()
    for route in routes_before:
        network = ipaddress.ip_network(route["DestinationPrefix"], strict=False)
        if network.prefixlen:
            assert not any(ipaddress.ip_address(address) in network for address in ADDRESSES + TARGETS), "dedicated test prefix overlaps an existing non-default route"
    evidence = {"started_ms": str(int(time.time() * 1000)), "production_before": before,
                "owned_name": name, "host_routes": PREFIXES, "checks": []}
    state, configs, runtime = (args.output_dir / part for part in ("state", "configs", "runtime"))
    endpoint = state / "management.sock"
    manager_config = args.output_dir / "manager.toml"
    manager_config.write_text(
        'role="agent"\n[storage]\n' + "\n".join(f"{key}={json.dumps(str(value))}" for key, value in (
            ("state_dir", state), ("config_dir", configs), ("runtime_dir", runtime), ("bin_dir", args.bin_dir)))
        + f'\n[upstream]\nenabled=false\nagent_id="{AGENT}"\n[local_ipc]\nendpoint={json.dumps(str(endpoint))}\n', encoding="utf-8")
    manager_log = (args.output_dir / "manager.log").open("w", encoding="utf-8")
    manager = None
    try:
        with upstream(True) as server:
            manager = subprocess.Popen([str(args.bin_dir / "astra-manager.exe"), "--config", str(manager_config)],
                stdout=manager_log, stderr=subprocess.STDOUT,
                env=dict(os.environ, ASTRA_WINTUN_DLL=str(args.wintun), ASTRA_RUNTIME_DIR=str(runtime)))
            deadline = time.monotonic() + 20
            while True:
                assert manager.poll() is None, "isolated Agent exited"
                try:
                    hello = query(args, endpoint, "hello", agent=AGENT)
                    assert "network_diagnostics_path_v1" in hello["capabilities"]
                    break
                except (subprocess.SubprocessError, RuntimeError):
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(.1)
            profile = args.output_dir / "tun.toml"
            profile.write_text(base(server, free_port()) + f'\n[tun]\nname="{name}"\naddress="{ADDRESSES[0]}"\nprefix=30\nipv6_address="{ADDRESSES[1]}"\nipv6_prefix=126\nmtu=1500\ninterface="{args.physical_interface}"\nroutes={json.dumps(PREFIXES)}\n', encoding="utf-8")
            execute(args, endpoint, "add", {"name": "tun-fixture", "kind": "proxy", "config": str(profile)})
            instance = next(item["id"] for item in query(args, endpoint, "instances", agent=AGENT) if item["name"] == "tun-fixture")
            execute(args, endpoint, "start", {"instance": instance})
            actual = query(args, endpoint, "runtime", {"instance": instance}, AGENT)["snapshot"]
            # Manager startup intentionally leaves TUN disabled. Activate only
            # this new fixture using the explicit local operation and its PID.
            if not actual["tun"]:
                rpc(args, endpoint, {"type": "local", "data": {"type": "enable_tun", "data": {
                    "instance": instance, "expected_pid": int(actual["pid"]),
                }}}, AGENT)
                actual = query(args, endpoint, "runtime", {"instance": instance}, AGENT)["snapshot"]
            core = query(args, endpoint, "network_core", {"instance": instance}, AGENT)
            assert actual["tun"] is True and core["status"] == "ok", (actual["tun"], core)
            assert str(core["identity"]["pid"]) == str(actual["pid"])
            evidence["runtime"] = {key: actual[key] for key in ("pid", "tun", "mode", "system_proxy")}
            evidence["core"] = core
            evidence["runtime_sampled_ms"] = str(int(time.time() * 1000))
            assert len(owned_routes()) == 2
            network = query(args, endpoint, "network_state", agent=AGENT)
            physical = next(item for item in network["interfaces"] if item["id"] == args.physical_interface)
            virtual = next(item for item in network["interfaces"] if item["name"] == name)
            assert physical["connected"] and physical["kind"] == "physical"
            assert virtual["kind"] != "physical"
            evidence["interfaces"] = {"physical": physical, "tun": virtual}
            evidence["checks"].append("actual Runtime.tun and core identity agree; two owned host routes exist")
            evidence["control_echo"] = []
            for family, target in zip(("ipv4", "ipv6"), TARGETS):
                ping = subprocess.run(["ping.exe", "-4" if family == "ipv4" else "-6", "-n", "1", "-w", "1000", target], capture_output=True, timeout=5)
                evidence["control_echo"].append({"family": family, "target": target, "reply": ping.returncode == 0})
                observed = probe(args, endpoint, instance, {"kind": "icmp", "host": target}, family)
                context = next(event for event in observed["events"] if event["event"] == "context")
                assert context["running_core"]["boot_id"] == core["identity"]["boot_id"]
                assert context["config_digest"] == core["identity"]["applied_digest"]
                echoes = [event["observation"] for event in observed["events"] if event["event"] == "echo"]
                assert len(echoes) == 1 and echoes[0]["tun_local"] and not echoes[0]["proves_reachability"]
                assert echoes[0]["rtt_ms"] is None and echoes[0]["from"] is None
                assert not any(event["event"] in ("resolution", "connection", "attempt") for event in observed["events"])
                observed["family"] = family
                evidence.setdefault("icmp", []).append(observed)
            traced = probe(args, endpoint, instance, {"kind": "trace", "host": TARGETS[0], "max_hops": 1, "probes_per_hop": 1})
            evidence["trace"] = traced
            summary = next(event for event in traced["events"] if event["event"] == "trace_summary")
            assert int(summary["probes_sent"]) == 0 and not summary["reached"]
            assert not any(event["event"] in ("resolution", "trace", "connection") for event in traced["events"])
            evidence["trace"] = traced
            virtual_request = {"kind": "nat", "request": {"agent_id": AGENT, "interface_id": virtual["id"], "family": "ipv4", "servers": [ADDRESSES[0] + ":3478"], "advanced": False}}
            try:
                probe(args, endpoint, instance, virtual_request, egress={"kind": "interface", "name": virtual["id"]})
                raise AssertionError("virtual NAT interface was accepted")
            except RuntimeError as error:
                assert "physical interface" in str(error), str(error)
                evidence["virtual_nat_rejection"] = str(error)
            physical_request = {"kind": "nat", "request": {"agent_id": AGENT, "interface_id": physical["id"], "family": "ipv4", "servers": [ADDRESSES[0] + ":3478"], "advanced": False}}
            nat = probe(args, endpoint, instance, physical_request, egress={"kind": "interface", "name": physical["id"]})
            assert nat["state"] in ("failed", "partial"), nat
            assert not any(event["event"] == "nat" and event.get("kind") == "observation" for event in nat["events"])
            evidence["physical_nat_local_target_rejection"] = nat
            evidence["checks"].append("known TUN echo has no RTT/reachability; System trace sends zero probes; NAT rejects virtual interface and local TUN target")
            evidence["upstream_requests"] = len(server.hits)
            assert not server.hits, "guarded diagnostics unexpectedly used the test proxy"
    finally:
        if manager is not None:
            try:
                if manager.poll() is None:
                    execute(args, endpoint, "shutdown")
                    manager.wait(timeout=25)
            finally:
                if manager.poll() is None:
                    subprocess.run([str(args.bin_dir / "astra-proxy.exe"), "--stop", str(state / "tun-fixture.sock")], capture_output=True, timeout=15)
                    manager.terminate()
                    manager.wait(timeout=10)
        manager_log.close()
        deadline = time.monotonic() + 15
        while owned_routes() and time.monotonic() < deadline:
            time.sleep(.2)
        evidence["routes_after"] = owned_routes()
        evidence["adapter_after"] = ps_json(f"@(Get-NetAdapter -Name '{name}' -ErrorAction SilentlyContinue | Select-Object ifIndex)")
        evidence["production_after"] = production(args)
        evidence["existing_routes_unchanged"] = route_snapshot() == routes_before
        (args.output_dir / "evidence.json").write_text(json.dumps(evidence, indent=2), encoding="utf-8")
        assert not evidence["routes_after"] and not evidence["adapter_after"], "owned network state was not removed"
        assert evidence["production_after"] == before, "production runtime changed"
        assert evidence["existing_routes_unchanged"], "existing route snapshot changed"
    print(json.dumps({"passed": evidence["checks"], "control_echo": evidence["control_echo"], "evidence": str(args.output_dir / "evidence.json")}), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("bin-dir", "output-dir", "rpc-helper", "wintun", "production-endpoint"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--physical-interface", required=True)
    parser.add_argument("--production-instance", required=True)
    run(parser.parse_args())
