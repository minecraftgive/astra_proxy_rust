// @vitest-environment jsdom
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as API from "../api";
import NetworkTools from "./NetworkTools";
import type {
  NetworkCoreReply,
  NetworkReport,
  NetworkResult,
} from "../generated/management";

vi.mock("../api", async (original) => ({
  ...(await original<typeof API>()),
  query: vi.fn(),
  request: vi.fn(),
}));
const props = { agent: "agent-a", instance: "proxy", online: true };
let capabilities: string[];
let reportState: NetworkReport["state"];
let reportItems: NetworkResult[];
let coreReply: NetworkCoreReply;
let runtimeSnapshot: Record<string, unknown>;
const flush = async () => {
  await act(async () => {
    await Promise.resolve();
  });
};
const advance = async (ms: number) => {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
};
const report = (id: string, cursor: string | null): NetworkReport => {
  const items = cursor ? [] : reportItems;
  return {
    operation_id: id,
    schema_version: 1,
    revision: "1",
    state: reportState,
    message: "Fixture report",
    started_ms: "1000",
    finished_ms: reportState === "running" ? null : "2000",
    truncated: false,
    expired: false,
    items,
    cursor: cursor ?? String(items.length),
    has_more: false,
  };
};
beforeEach(() => {
  vi.useFakeTimers();
  sessionStorage.clear();
  vi.mocked(API.query).mockReset();
  vi.mocked(API.request).mockReset();
  vi.spyOn(document, "hidden", "get").mockReturnValue(false);
  capabilities = [
    "network_diagnostics_jobs_v1",
    "network_diagnostics_tools_v1",
    "network_diagnostics_nat_v1",
    "network_diagnostics_path_v1",
    "network_diagnostics_batch_v1",
    "network_state_v1",
    "network_diagnostics_resolution_v1",
    "network_diagnostics_pinned_v1",
    "core_network_v1",
  ];
  reportState = "running";
  reportItems = [];
  coreReply = {
    schema_version: 1,
    instance: "proxy",
    sampled_at_ms: "1000",
    status: "ok",
    message: null,
    identity: {
      version: 1,
      boot_id: "core-boot-1",
      pid: 42,
      applied_digest: "digest-1",
      capabilities: ["fake_ip_lookup_v1"],
    },
  };
  runtimeSnapshot = {
    pid: 42,
    mode: "rule",
    tun: true,
    system_proxy: false,
    groups: [
      { name: "PROXY", mode: "select", selected: "香港" },
      { name: "香港", mode: "url-test", selected: "hk-01" },
    ],
  };
  vi.mocked(API.query).mockImplementation(async (agent, query, data) => {
    if (query === "hello")
      return {
        type: "hello",
        data: {
          version: 2,
          role: "agent",
          epoch: "boot",
          build_id: "fixture",
          capabilities,
        },
      };
    if (query === "network_state")
      return {
        type: "network_state",
        data: {
          schema_version: 1,
          agent_id: agent,
          sampled_at_ms: "1000",
          dns_sampled_at_ms: "1000",
          interfaces: [
            {
              id: "en0",
              name: "en0",
              index: 3,
              kind: "physical",
              connected: true,
              addresses: [{ address: "192.0.2.1", prefix_len: 24 }],
              source: "fixture",
            },
          ],
          dns: [],
          routes: [
            {
              destination: "default",
              gateway: "192.0.2.254",
              interface_id: "en0",
              flags: [],
              source: "fixture",
            },
          ],
          issues: [],
          truncated: false,
        },
      };
    if (query === "runtime")
      return {
        type: "runtime",
        data: { kind: "client", snapshot: structuredClone(runtimeSnapshot) },
      };
    if (query === "node_tags") return { type: "node_tags", data: ["hk-01"] };
    if (query === "network_core")
      return { type: "network_core", data: structuredClone(coreReply) };
    if (query === "fake_ip_lookup")
      return {
        type: "fake_ip_lookup",
        data: {
          schema_version: 1,
          instance: "proxy",
          sampled_at_ms: "2000",
          status: "ok",
          message: null,
          boot_id: coreReply.identity?.boot_id,
          mappings: (data as { addresses: string[] }).addresses.map(
            (address) => ({
              address,
              domain: address === "198.18.0.5" ? "example.com" : null,
            }),
          ),
        },
      };
    if (query === "network_report") {
      const d = data as { operation_id: string; cursor: string | null };
      return { type: "network_report", data: report(d.operation_id, d.cursor) };
    }
    throw new Error(`Unexpected query ${query}`);
  });
  vi.mocked(API.request).mockImplementation(async (_agent, body) => {
    const data = body.data as { operation_id: string };
    return { type: "task", data: { id: data.operation_id, state: "running" } };
  });
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.useRealTimers();
});
const tool = (name: string) =>
  fireEvent.click(screen.getByRole("radio", { name }));
const submitted = () =>
  (vi.mocked(API.request).mock.calls[0][1].data as any).command;

it("shows the live link chain for the TUN-captured system route", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  const chain = screen.getByTestId("network-live-chain");
  expect(chain.textContent).toBe(
    "当前链路系统›TUN›规则›PROXY → 香港›香港 → hk-01›hk-01›网卡 en0（默认路由）",
  );
  tool("TCP 连接");
  fireEvent.change(screen.getByLabelText("出口"), {
    target: { value: "outbound:香港" },
  });
  expect(screen.getByTestId("network-live-chain").textContent).toContain(
    "香港 → hk-01",
  );
  expect(screen.getByTestId("network-live-chain").textContent).not.toContain(
    "规则",
  );
});

it("runs a multi-port TCP probe as one background job", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  tool("TCP 连接");
  fireEvent.change(screen.getByLabelText("目标"), {
    target: { value: "example.com:80,443" },
  });
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  await flush();
  const command = submitted();
  expect(command.type).toBe("network_diagnostics");
  expect(command.data.request.probe).toEqual({
    kind: "tcp_ports",
    host: "example.com",
    ports: [80, 443],
  });
  expect(screen.getByRole("button", { name: "取消" })).toBeTruthy();
});

it("trace only offers egresses that can reveal the physical path", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  tool("逐跳路径");
  await flush();
  const options = [
    ...(screen.getByLabelText("出口") as HTMLSelectElement).options,
  ].map((o) => o.value);
  expect(options).toEqual(["iface:en0", "outbound:DIRECT"]);
});

it("an Agent without the job interface is told to update and nothing is sent", async () => {
  capabilities = ["network_state_v1"];
  render(<NetworkTools {...props} />);
  await flush();
  expect(screen.getByRole("status").textContent).toContain("版本过旧");
  expect(
    (screen.getByRole("button", { name: "运行" }) as HTMLButtonElement)
      .disabled,
  ).toBe(true);
  expect(API.request).not.toHaveBeenCalled();
});

it("NAT requires a physical interface, then marks the detected row", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  tool("NAT 类型");
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(screen.getByRole("alert").textContent).toContain("请选择物理网卡");
  fireEvent.change(screen.getByLabelText("物理网卡"), {
    target: { value: "en0" },
  });
  reportState = "complete";
  reportItems = [
    {
      id: "1",
      sampled_at_ms: "1500",
      data: {
        event: "nat",
        kind: "report",
        report: {
          interface_name: "en0",
          physical_path: { binding_verified: true, route_verified: true },
          traditional_type: "port_restricted_cone_observed",
          advanced_capable: true,
          unstable: false,
          partial: false,
          mapping: "endpoint_independent",
          filtering: "address_and_port_dependent",
          local_endpoint: "192.0.2.1:5000",
          observations: [{ mapped_endpoint: "203.0.113.7:5000" }],
          notes: [],
        },
      },
    },
  ];
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  await advance(800);
  const command = submitted();
  expect(command.data.request.egress).toEqual({
    kind: "interface",
    name: "en0",
  });
  expect(command.data.request.probe.request.interface_id).toBe("en0");
  const detected = document.querySelector("tr.is-detected");
  if (!detected) throw new Error(document.body.textContent ?? "");
  expect(detected.textContent).toContain("端口受限锥型");
  expect(detected.textContent).toContain("一般");
  expect(screen.getByTestId("network-live-chain").textContent).toContain(
    "NAT：端口受限锥型",
  );
  expect((screen.getByLabelText("物理网卡") as HTMLSelectElement).value).toBe(
    "",
  );
});

it("full check submits a fixed batch with comparison and NAT", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  tool("完整检查");
  fireEvent.change(screen.getByLabelText("对照出口"), {
    target: { value: "outbound:DIRECT" },
  });
  fireEvent.click(screen.getByLabelText("两轮对照"));
  fireEvent.click(screen.getByLabelText("包含 NAT 检测"));
  fireEvent.change(screen.getByLabelText("物理网卡"), {
    target: { value: "en0" },
  });
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  await flush();
  const command = submitted();
  expect(command.type).toBe("network_diagnostics_batch");
  expect(command.data.request.comparison).toBe(true);
  const paths = new Set(
    command.data.request.projects.map((p: any) => p.path_id),
  );
  expect([...paths]).toEqual(["path-a", "path-b", "path-nat"]);
});

it("results stay in one column with backend findings and the probe path", async () => {
  reportState = "complete";
  reportItems = [
    {
      id: "1",
      sampled_at_ms: "1",
      data: {
        event: "attempt",
        seq: "1",
        rtt_ms: 12.5,
        detail: "reply",
        error: null,
      },
    },
    {
      id: "2",
      sampled_at_ms: "1",
      data: {
        event: "summary",
        sent: "1",
        received: "1",
        min_ms: 12.5,
        avg_ms: 12.5,
        max_ms: 12.5,
      },
    },
    { id: "3", sampled_at_ms: "1", data: { event: "removed_kind", x: 1 } },
    {
      id: "4",
      sampled_at_ms: "1",
      data: {
        event: "analysis",
        findings: [
          {
            severity: "risk",
            confidence: "high",
            title: "明文 DNS",
            explanation: "UDP 53",
            recommendation: "使用 DoH",
          },
        ],
        notes: [],
        topology: [
          {
            path_id: "path-a",
            truncated: false,
            nodes: [
              { id: "agent", label: "目标 Agent", role: "agent" },
              { id: "t", label: "example.com", role: "target" },
            ],
            edges: [
              {
                from: "agent",
                to: "t",
                section: "connection",
                level: "measured",
                description: "实际连接",
                evidence: [],
              },
            ],
            attachments: [],
          },
        ],
      },
    },
  ];
  render(<NetworkTools {...props} />);
  await flush();
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  await advance(800);
  const log = screen.getByRole("log").textContent ?? "";
  expect(log).toContain("12.50 ms");
  expect(log).toContain("发送 1 · 接收 1 · 丢失 0%");
  expect(log).toContain("[风险 · 置信度高] 明文 DNS：UDP 53");
  expect(log).not.toContain("removed_kind");
  fireEvent.click(screen.getByRole("button", { name: "─›" }));
  expect(
    screen.getByRole("region", { name: "路径证据" }).textContent,
  ).toContain("实际连接");
});

it("Fake-IP lookup checks the core capability before querying", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  tool("Fake-IP");
  fireEvent.change(screen.getByLabelText("IP 地址"), {
    target: { value: "198.18.0.5, example.com" },
  });
  fireEvent.click(screen.getByRole("button", { name: "查询映射" }));
  await flush();
  expect(screen.getByText(/有效 IPv4/)).toBeTruthy();
  fireEvent.change(screen.getByLabelText("IP 地址"), {
    target: { value: "198.18.0.5 198.18.0.6" },
  });
  fireEvent.click(screen.getByRole("button", { name: "查询映射" }));
  await flush();
  await flush();
  expect(screen.getByText("198.18.0.5 → example.com")).toBeTruthy();
  expect(API.request).not.toHaveBeenCalled();
});

it("a vanished adapter stays selected and is rejected instead of falling back", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  tool("TCP 连接");
  fireEvent.change(screen.getByLabelText("出口"), {
    target: { value: "iface:en0" },
  });
  const original = vi.mocked(API.query).getMockImplementation()!;
  vi.mocked(API.query).mockImplementation(
    async (agent, query, data, signal) => {
      const reply = await original(agent, query, data, signal);
      if (query === "network_state") (reply.data as any).interfaces = [];
      return reply;
    },
  );
  await advance(5100);
  const select = screen.getByLabelText("出口") as HTMLSelectElement;
  expect(select.value).toBe("iface:en0");
  expect(select.selectedOptions[0].textContent).toContain("不可用");
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(screen.getByRole("alert").textContent).toContain("所选出口已不可用");
  expect(API.request).not.toHaveBeenCalled();
});
