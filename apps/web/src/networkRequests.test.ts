import { expect, it } from "vitest";
import type { Command, Egress, NetworkDiagnosticsRequest, Probe } from "./generated/management";
import fixtures from "../../../crates/astra-management/src/test/fixtures/network_requests.json";

// Every generated discriminant must stay expressible without unknown/any casts.
// The same fixtures are decoded by Rust and exercise both command envelopes.
const probes = {
  dns: { kind: "dns", name: "example.test", record: "A", server: "tcp://192.0.2.53:53" },
  tcp: { kind: "tcp", host: "example.test", port: 443 },
  icmp: { kind: "icmp", host: "192.0.2.1" },
  tcp_ports: { kind: "tcp_ports", host: "example.test", ports: [80, 443] },
  tls: { kind: "tls", host: "192.0.2.1", port: 443, server_name: "example.test", compare_bundled: true },
  http: { kind: "http", url: "https://example.test/", method: "HEAD", compare_bundled: false },
  https_ip: { kind: "https_ip", url: "https://example.test/ip", compare_bundled: false },
  stun: { kind: "stun", server: "stun:192.0.2.1:3478" },
  nat: { kind: "nat", request: { agent_id: "agent-test", interface_id: "eth0", family: "ipv4", servers: ["192.0.2.1:3478"], advanced: false } },
  trace: { kind: "trace", host: "192.0.2.1", max_hops: 30, probes_per_hop: 3 },
} satisfies Record<Probe["kind"], Probe>;
const requests: NetworkDiagnosticsRequest[] = Object.values(probes).map((probe) => ({
  resolver: null, family: null,
  egress: probe.kind === "nat" || probe.kind === "trace" ? { kind: "interface", name: "eth0" } : { kind: "system" },
  probe, count: 1, interval_ms: "0", timeout_ms: "1000",
}));

it("matches the shared Rust JSON contract for every typed diagnostic probe", () => {
  expect(requests).toEqual(fixtures);
  for (const request of requests) {
    const commands: Command[] = [
      { type: "network_diagnostics", data: { instance: "proxy", request } },
      { type: "network_diagnostics_batch", data: { instance: "proxy", request: { comparison: false, projects: [{ id: "probe", path_id: "selected", request }] } } },
    ];
    expect(JSON.parse(JSON.stringify(commands))).toEqual(commands);
  }
});

// These assertions are checked by tsc, not by a runtime cast.
// @ts-expect-error TCP needs a destination port.
const missingPort: Probe = { kind: "tcp", host: "example.test" };
// @ts-expect-error Counters crossing the management JSON boundary are strings.
const numericTimeout: NetworkDiagnosticsRequest = { ...requests[0], timeout_ms: 1000 };
// @ts-expect-error The public envelope cannot inject manager physical-path evidence.
const forgedContext: NetworkDiagnosticsRequest = { ...requests[0], path_context: { core_tun: false } };
// @ts-expect-error Unknown discriminants are not typed requests.
const unknownProbe: Probe = { kind: "future_probe", host: "example.test" };
const outbound: Egress = { kind: "outbound", name: "selected-group" };
void [missingPort, numericTimeout, forgedContext, unknownProbe, outbound];
