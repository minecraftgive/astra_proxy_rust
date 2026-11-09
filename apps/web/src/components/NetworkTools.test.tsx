// @vitest-environment jsdom
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  within,
} from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as API from "../api";
import NetworkTools, {
  DiagnosticResult,
  DiagnosticColumns,
} from "./NetworkTools";
import { conclusions, latestConclusions } from "./NetworkConclusions";
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
const report = (id: string): NetworkReport => ({
  operation_id: id,
  schema_version: 1,
  revision: "1",
  state: reportState,
  message: "Fixture report",
  started_ms: "1000",
  finished_ms: reportState === "running" ? null : "2000",
  truncated: false,
  expired: false,
  items: [],
  cursor: "0",
  has_more: false,
});
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
    "network_diagnostics_interface_ip_v1",
    "network_diagnostics_resolution_v1",
    "network_diagnostics_pinned_v1",
    "core_network_v1",
  ];
  reportState = "running";
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
    groups: [],
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
              id: `${agent}-nic`,
              name: `${agent} physical NIC`,
              index: 3,
              kind: "physical",
              connected: true,
              addresses: [{ address: "192.0.2.1", prefix_len: 24 }],
              source: "fixture",
            },
          ],
          dns: [],
          routes: [],
          issues: [],
          truncated: false,
        },
      };
    if (query === "runtime")
      return {
        type: "runtime",
        data: {
          kind: "client",
          snapshot: structuredClone(runtimeSnapshot),
        },
      };
    if (query === "node_tags")
      return { type: "node_tags", data: ["proxy-node"] };
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
            (address) => ({ address, domain: null }),
          ),
        },
      };
    if (query === "network_report")
      return {
        type: "network_report",
        data: report(String((data as API.Data | undefined)?.operation_id)),
      };
    throw new Error(`Unexpected query ${query}`);
  });
  vi.mocked(API.request).mockImplementation(async (_agent, body) => {
    const data = body.data as {
      operation_id: string;
      command: { type: string };
    };
    return { type: "task", data: { id: data.operation_id, state: "running" } };
  });
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.useRealTimers();
});
async function toolbox() {
  fireEvent.click(screen.getByRole("button", { name: "工具箱" }));
  await flush();
}

async function chooseTool(kind: string) {
  await toolbox();
  fireEvent.change(screen.getByLabelText("工具"), { target: { value: kind } });
}
const submittedRequest = () =>
  (vi.mocked(API.request).mock.calls[0][1].data as any).command.data.request;

it("new interface TLS hostnames still require the Agent resolution capability", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  await chooseTool("tls");
  fireEvent.change(screen.getByLabelText("本次探测出口"), {
    target: { value: "iface:agent-a-nic" },
  });
  capabilities = capabilities.filter(
    (cap) => cap !== "network_diagnostics_resolution_v1",
  );
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(API.request).not.toHaveBeenCalled();
  expect(screen.getByRole("alert").textContent).toContain("未声明新版解析能力");
});

it("malformed result pages remain a protocol error and never trigger execution replay", async () => {
  const query = vi.mocked(API.query).getMockImplementation()!;
  vi.mocked(API.query).mockImplementation(async (...args) =>
    args[1] === "network_report"
      ? {
          type: "network_report",
          data: {
            ...report(String((args[2] as any).operation_id)),
            items: [{ id: "1", sampled_at_ms: "1000", data: null }],
          },
        }
      : query(...args),
  );
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  await advance(800);
  expect(screen.getByRole("alert").textContent).toContain(
    "协议错误：报告状态、游标或结果事件结构无效",
  );
  expect(API.request).toHaveBeenCalledTimes(1);
});

it("NAT requires a new manual physical interface, ignores generic egress, and clears after submit", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.change(screen.getByLabelText("本次探测出口"), {
    target: { value: "outbound:proxy-node" },
  });
  await chooseTool("nat");
  expect(
    (screen.getByLabelText("NAT 物理网卡") as HTMLSelectElement).value,
  ).toBe("");
  expect(screen.getByText(/服务会看到该网络的公网出口 IP/)).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(screen.getByRole("alert").textContent).toContain("请选择物理网卡");
  expect(API.request).not.toHaveBeenCalled();
  fireEvent.change(screen.getByLabelText("NAT 物理网卡"), {
    target: { value: "agent-a-nic" },
  });
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(submittedRequest()).toMatchObject({
    egress: { kind: "interface", name: "agent-a-nic" },
    family: "ipv4",
    count: 1,
    timeout_ms: "30000",
    probe: {
      kind: "nat",
      request: {
        agent_id: "agent-a",
        interface_id: "agent-a-nic",
        family: "ipv4",
        advanced: true,
        servers: ["stun.cloudflare.com:3478", "stun.l.google.com:19302"],
      },
    },
  });
  expect(
    (screen.getByLabelText("NAT 物理网卡") as HTMLSelectElement).value,
  ).toBe("");
});

it("clears NAT interface on family and Agent changes", async () => {
  const view = render(<NetworkTools {...props} />);
  await flush();
  await chooseTool("nat");
  fireEvent.change(screen.getByLabelText("NAT 物理网卡"), {
    target: { value: "agent-a-nic" },
  });
  fireEvent.change(screen.getByLabelText("地址族"), {
    target: { value: "ipv6" },
  });
  expect(
    within(screen.getByLabelText("NAT 物理网卡")).getAllByRole("option"),
  ).toHaveLength(1);
  fireEvent.change(screen.getByLabelText("地址族"), {
    target: { value: "ipv4" },
  });
  fireEvent.change(screen.getByLabelText("NAT 物理网卡"), {
    target: { value: "agent-a-nic" },
  });
  view.rerender(<NetworkTools {...props} agent="agent-b" />);
  await flush();
  expect(
    (screen.getByLabelText("NAT 物理网卡") as HTMLSelectElement).value,
  ).toBe("");
  expect(API.request).not.toHaveBeenCalled();
});

it.each([
  [
    "tls",
    {
      kind: "tls",
      host: "example.com",
      port: 443,
      server_name: null,
      compare_bundled: false,
    },
    null,
    "3000",
  ],
  [
    "http",
    {
      kind: "http",
      url: "https://example.com/",
      method: "HEAD",
      compare_bundled: false,
    },
    null,
    "3000",
  ],
  [
    "https_ip",
    { kind: "https_ip", url: "https://api.ipify.org", compare_bundled: false },
    null,
    "3000",
  ],
  ["stun", { kind: "stun", server: "stun.cloudflare.com:3478" }, null, "3000"],
  [
    "trace",
    { kind: "trace", host: "example.com", max_hops: 30, probes_per_hop: 3 },
    null,
    "1000",
  ],
  ["icmp", { kind: "icmp", host: "example.com" }, null, "1000"],
  ["ipv6", { kind: "ipv6", host: "example.com", port: 443 }, "ipv6", "3000"],
])(
  "constructs a typed %s request only after Run",
  async (kind, probe, family, timeout_ms) => {
    render(<NetworkTools {...props} />);
    await flush();
    await chooseTool(String(kind));
    expect(API.request).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "运行" }));
    await flush();
    expect(submittedRequest()).toMatchObject({
      probe,
      family,
      timeout_ms,
      count: 1,
    });
  },
);

it("bounds TCP ports and sends deduplicated valid ports", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  await chooseTool("tcp_ports");
  fireEvent.change(screen.getByLabelText("端口（最多 16 个）"), {
    target: { value: Array.from({ length: 17 }, (_, i) => i + 1).join(",") },
  });
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(API.request).not.toHaveBeenCalled();
  fireEvent.change(screen.getByLabelText("端口（最多 16 个）"), {
    target: { value: "80,443,443" },
  });
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(submittedRequest().probe).toEqual({
    kind: "tcp_ports",
    host: "example.com",
    ports: [80, 443],
  });
});

it.each([
  ["tls", "network_diagnostics_tools_v1"],
  ["trace", "network_diagnostics_path_v1"],
  ["nat", "network_diagnostics_nat_v1"],
])("rechecks latest capability for %s", async (kind, cap) => {
  render(<NetworkTools {...props} />);
  await flush();
  await chooseTool(kind);
  if (kind === "nat")
    fireEvent.change(screen.getByLabelText("NAT 物理网卡"), {
      target: { value: "agent-a-nic" },
    });
  capabilities = capabilities.filter((x) => x !== cap);
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(API.request).not.toHaveBeenCalled();
  expect(screen.getByRole("alert").textContent).toContain("未提交任务");
});

it("submits a batch without implicit comparison and keeps each project on its path", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  fireEvent.click(screen.getByRole("button", { name: "诊断" }));
  await flush();
  expect(API.request).not.toHaveBeenCalled();
  fireEvent.change(screen.getByLabelText("路径 1 出口"), {
    target: { value: "outbound:proxy-node" },
  });
  fireEvent.click(screen.getByRole("button", { name: "运行完整诊断" }));
  await flush();
  const request = submittedRequest();
  expect(request.comparison).toBe(false);
  expect(request.projects).toHaveLength(7);
  expect(
    request.projects.every(
      (p: any) =>
        p.path_id === "path1" && p.request.egress.name === "proxy-node",
    ),
  ).toBe(true);
  expect(
    (vi.mocked(API.request).mock.calls[0][1].data as any).command.type,
  ).toBe("network_diagnostics_batch");
});

it("batch NAT is independently mandatory and can be excluded without blocking ordinary projects", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  fireEvent.click(screen.getByRole("button", { name: "诊断" }));
  await flush();
  fireEvent.click(screen.getByLabelText(/包含 NAT 检测/));
  fireEvent.click(screen.getByRole("button", { name: "运行完整诊断" }));
  await flush();
  expect(API.request).not.toHaveBeenCalled();
  expect(screen.getByRole("alert").textContent).toContain("请选择物理网卡");
  fireEvent.click(screen.getByLabelText(/包含 NAT 检测/));
  fireEvent.click(screen.getByLabelText(/对照检测：/));
  fireEvent.click(screen.getByRole("button", { name: "运行完整诊断" }));
  await flush();
  expect(submittedRequest().comparison).toBe(true);
  expect(
    submittedRequest().projects.some(
      (p: any) => p.request.probe.kind === "nat",
    ),
  ).toBe(false);
});

it("batch mode rejects an older Agent instead of replaying projects separately", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  fireEvent.click(screen.getByRole("button", { name: "诊断" }));
  await flush();
  capabilities = capabilities.filter(
    (x) => x !== "network_diagnostics_batch_v1",
  );
  fireEvent.click(screen.getByRole("button", { name: "运行完整诊断" }));
  await flush();
  expect(API.request).not.toHaveBeenCalled();
  expect(screen.getByRole("alert").textContent).toContain("批量诊断能力");
});

it("renders wrapped TLS failure evidence without calling it successful or requiring a new request", () => {
  render(
    <DiagnosticResult
      advanced
      item={{
        id: "1",
        sampled_at_ms: "1000",
        data: {
          event: "project",
          project: { project_id: "tls", path_id: "path2", round: 2, index: 3 },
          detail: {
            event: "tls",
            seq: 1,
            detail: {
              kind: "handshake",
              server_name: "example.com",
              verified: false,
              verification_attempted: true,
              verification_error: "UnknownIssuer",
              received_certificates: 1,
              retained_certificates: 1,
              retained_der_bytes: 10,
              handshake_ms: 2,
            },
          },
        },
      }}
    />,
  );
  expect(screen.getByText(/项目 tls · 路径 path2 · 第 2 轮/)).toBeTruthy();
  expect(screen.getByText("UnknownIssuer")).toBeTruthy();
  expect(screen.getByText("失败")).toBeTruthy();
});

it("TUN-local echo never claims target reachability or displays its latency as valid", () => {
  render(
    <DiagnosticResult
      advanced
      item={{
        id: "1",
        sampled_at_ms: "1000",
        data: {
          event: "echo",
          observation: {
            target: "example.com",
            family: "ipv6",
            tun_local: true,
            proves_reachability: false,
            outcome: "tun_local",
            rtt_ms: 0.1,
          },
        },
      }}
    />,
  );
  expect(screen.getByText("不计入统计")).toBeTruthy();
  expect(screen.getByText("无法据此确认")).toBeTruthy();
});

it("renders shared findings and keyboard-clickable topology with explicit unknown edges", () => {
  render(
    <DiagnosticResult
      advanced
      item={{
        id: "1",
        sampled_at_ms: "1000",
        data: {
          event: "analysis",
          rules_version: 1,
          comparison_status: "未请求对照",
          incomplete: true,
          findings: [
            {
              title: "fixture TLS error",
              confidence: "high",
              severity: "needs_verification",
              explanation: "验证器返回错误",
              alternatives: ["企业根证书"],
              recommendation: "检查信任来源",
              evidence: [],
            },
          ],
          topology: [
            {
              path_id: "p1",
              nodes: [
                { id: "a", label: "代理" },
                { id: "b", label: "目标" },
              ],
              edges: [
                {
                  from: "a",
                  to: "b",
                  section: "connection",
                  level: "unknown",
                  description: "代理后路径不可见",
                  evidence: [],
                },
              ],
              attachments: [],
            },
          ],
          visibility: [],
        },
      }}
    />,
  );
  expect(screen.getByText(/可信程度：高 · 严重程度：待核实/)).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: /代理 → 目标/ }));
  expect(screen.getByText("代理后路径不可见")).toBeTruthy();
  expect(API.request).not.toHaveBeenCalled();
});

it("only samples local OS/runtime data until the user explicitly runs a probe", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  expect(screen.getByText(/网卡：agent-a physical NIC/)).toBeTruthy();
  await advance(10000);
  expect(API.request).not.toHaveBeenCalled();
  expect(
    vi
      .mocked(API.query)
      .mock.calls.every((call) =>
        [
          "hello",
          "network_state",
          "runtime",
          "node_tags",
          "network_core",
        ].includes(call[1]),
      ),
  ).toBe(true);
  await toolbox();
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(API.request).toHaveBeenCalledTimes(1);
  const [agent, body] = vi.mocked(API.request).mock.calls[0];
  expect(agent).toBe("agent-a");
  expect(body).toMatchObject({
    type: "execute",
    data: {
      command: {
        type: "network_diagnostics",
        data: {
          instance: "proxy",
          request: {
            egress: { kind: "system" },
            probe: { kind: "tcp", host: "example.com", port: 443 },
          },
        },
      },
    },
  });
});

it("keeps the original operation identity after a lost acceptance receipt without replay", async () => {
  vi.mocked(API.request).mockRejectedValue(
    new API.ApiError("outcome_unknown", "reply lost"),
  );
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  await advance(1500);
  expect(API.request).toHaveBeenCalledTimes(1);
  const body = vi.mocked(API.request).mock.calls[0][1].data as {
    operation_id: string;
  };
  const reads = vi
    .mocked(API.query)
    .mock.calls.filter((call) => call[1] === "network_report");
  expect(reads.length).toBeGreaterThan(0);
  expect(
    reads.every(
      (call) =>
        call[0] === "agent-a" &&
        (call[2] as API.Data | undefined)?.operation_id === body.operation_id,
    ),
  ).toBe(true);
  expect(sessionStorage.getItem("astra-network-job:agent-a:proxy")).toBe(
    body.operation_id,
  );
  expect(
    (screen.getByRole("button", { name: "运行" }) as HTMLButtonElement)
      .disabled,
  ).toBe(true);
});

it("clears interface selection and never queries an old task against another Agent", async () => {
  sessionStorage.setItem("astra-network-job:agent-a:proxy", "agent-a-job");
  const { rerender } = render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.change(screen.getByLabelText("本次探测出口"), {
    target: { value: "iface:agent-a-nic" },
  });
  rerender(<NetworkTools {...props} agent="agent-b" />);
  await flush();
  expect(
    (screen.getByLabelText("本次探测出口") as HTMLSelectElement).value,
  ).toBe("system");
  expect(
    screen.queryByRole("option", {
      name: "agent-a physical NIC (agent-a-nic)",
    }),
  ).toBeNull();
  expect(
    vi
      .mocked(API.query)
      .mock.calls.filter(
        (call) =>
          call[0] === "agent-b" &&
          call[1] === "network_report" &&
          (call[2] as API.Data | undefined)?.operation_id === "agent-a-job",
      ),
  ).toHaveLength(0);
  expect(API.request).not.toHaveBeenCalled();
});

it("does not report cancellation until a terminal report confirms it", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  fireEvent.click(screen.getByRole("button", { name: "取消任务" }));
  await flush();
  expect(screen.getByRole("button", { name: "等待确认取消…" })).toBeTruthy();
  expect(screen.queryByText("已取消")).toBeNull();
  const calls = vi.mocked(API.request).mock.calls;
  expect(calls).toHaveLength(2);
  const started = calls[0][1].data as { operation_id: string };
  expect(calls[1][1]).toMatchObject({
    type: "execute",
    data: {
      command: {
        type: "cancel_task",
        data: { operation_id: started.operation_id },
      },
    },
  });
  reportState = "cancelled";
  await advance(750);
  expect(screen.getByRole("button", { name: "取消任务" })).toBeTruthy();
  expect(
    (screen.getByRole("button", { name: "运行" }) as HTMLButtonElement)
      .disabled,
  ).toBe(false);
});

it("stopping observation stops polling without cancelling the Agent task", async () => {
  sessionStorage.setItem(
    "astra-network-job:agent-a:proxy",
    "still-running-job",
  );
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.click(screen.getByRole("button", { name: "停止观察" }));
  await flush();
  const reads = vi
    .mocked(API.query)
    .mock.calls.filter((call) => call[1] === "network_report").length;
  await advance(2000);
  expect(
    vi
      .mocked(API.query)
      .mock.calls.filter((call) => call[1] === "network_report"),
  ).toHaveLength(reads);
  expect(API.request).not.toHaveBeenCalled();
  expect(screen.getByRole("alert").textContent).toContain("still-running-job");
});

it("renders DNS records with section, class, TTL, and explicit truncation", () => {
  render(
    <DiagnosticResult
      advanced
      item={{
        id: "3",
        sampled_at_ms: "1000",
        data: {
          event: "dns",
          seq: "1",
          detail: {
            kind: "records",
            section: "authority",
            records: [
              {
                name: "example.test.",
                record_type: "SOA",
                class: "IN",
                ttl: "3600",
                text: "ns.example.test. hostmaster.example.test.",
                text_truncated: true,
              },
            ],
          },
        },
      }}
    />,
  );
  const table = screen.getByRole("table");
  expect(within(table).getByText("authority")).toBeTruthy();
  expect(within(table).getByText("SOA")).toBeTruthy();
  expect(within(table).getByText("IN")).toBeTruthy();
  expect(within(table).getByText("3600")).toBeTruthy();
  expect(within(table).getByText(/\[已截断\]/)).toBeTruthy();
});

it("finishes all result pages even when the first page already has a terminal state", async () => {
  sessionStorage.setItem("astra-network-job:agent-a:proxy", "finished-job");
  const original = vi.mocked(API.query).getMockImplementation()!;
  vi.mocked(API.query).mockImplementation(
    async (agent, query, data, signal) => {
      if (query !== "network_report")
        return original(agent, query, data, signal);
      const page = report("finished-job");
      page.state = "complete";
      page.finished_ms = "2000";
      if (!(data as API.Data | undefined)?.cursor) {
        page.has_more = true;
        page.cursor = "1";
        page.items = [
          {
            id: "1",
            sampled_at_ms: "1000",
            data: { event: "attempt", detail: "first page evidence" },
          },
        ];
        return { type: "network_report", data: page };
      }
      await new Promise((resolve) => setTimeout(resolve, 20));
      if (signal?.aborted) throw new DOMException("aborted", "AbortError");
      page.cursor = "2";
      page.items = [
        {
          id: "2",
          sampled_at_ms: "1001",
          data: { event: "attempt", detail: "second page evidence" },
        },
      ];
      return { type: "network_report", data: page };
    },
  );
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  await advance(30);
  fireEvent.click(screen.getByRole("button", { name: /显示原始报告/ }));
  expect(screen.getByText("first page evidence")).toBeTruthy();
  expect(screen.getByText("second page evidence")).toBeTruthy();
});

it("shows legacy limitations and only calls the old tool after clicking Run", async () => {
  capabilities = [];
  vi.mocked(API.request).mockResolvedValue({
    type: "net_tool",
    data: [{ event: "done" }],
  });
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  expect(screen.getByText(/旧 Agent：使用原工具/)).toBeTruthy();
  expect(API.request).not.toHaveBeenCalled();
  expect(
    (screen.getByRole("button", { name: "取消任务" }) as HTMLButtonElement)
      .disabled,
  ).toBe(true);
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(vi.mocked(API.request).mock.calls[0][1]).toMatchObject({
    type: "execute",
    data: { command: { type: "net_tool" } },
  });
  expect(
    vi
      .mocked(API.query)
      .mock.calls.some((call) => call[1] === "network_report"),
  ).toBe(false);
});

it.each([
  ["iface:agent-a-nic", "tcp://resolver.test:53", false],
  ["iface:agent-a-nic", "tcp://[2001:db8::53]:53", true],
  ["system", "udp://resolver.test#bootstrap=192.0.2.53", false],
  ["iface:agent-a-nic", "udp://[::1]trailer#bootstrap=192.0.2.53", false],
  ["system", "tcp://resolver.test:53", true],
])("legacy DNS admission %s / %s is %s", async (egress, server, allowed) => {
  render(<NetworkTools {...props} />);
  await flush();
  await chooseTool("dns");
  fireEvent.change(screen.getByLabelText("本次探测出口"), {
    target: { value: egress },
  });
  fireEvent.change(screen.getByLabelText("DNS 解析器"), {
    target: { value: server },
  });
  capabilities = ["network_state_v1"];
  vi.mocked(API.request).mockResolvedValue({ type: "net_tool", data: [] });
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(API.request).toHaveBeenCalledTimes(allowed ? 1 : 0);
  if (allowed)
    expect(vi.mocked(API.request).mock.calls[0][1]).toMatchObject({
      data: { command: { type: "net_tool" } },
    });
  else expect(screen.getByRole("alert").textContent).toContain("未启动探测");
});

it.each(["outbound:DIRECT", "outbound:proxy-node"])(
  "legacy rejects configuration egress %s after a fresh Hello",
  async (egress) => {
    render(<NetworkTools {...props} />);
    await flush();
    await toolbox();
    fireEvent.change(screen.getByLabelText("本次探测出口"), {
      target: { value: egress },
    });
    capabilities = [];
    fireEvent.click(screen.getByRole("button", { name: "运行" }));
    await flush();
    expect(API.request).not.toHaveBeenCalled();
    expect(screen.getByRole("alert").textContent).toContain("配置出口");
  },
);

it.each([false, true])(
  "single/batch (%s) receipt handling uses codes and never replays",
  async (batch) => {
    for (const code of [
      "unsupported",
      "invalid_request",
      "conflict",
      "outcome_unknown",
      "offline",
      "internal",
    ]) {
      const rejected = ["unsupported", "invalid_request", "conflict"].includes(
        code,
      );
      sessionStorage.clear();
      vi.mocked(API.query).mockClear();
      vi.mocked(API.request).mockClear();
      vi.mocked(API.request).mockRejectedValue(
        new API.ApiError(code, "Unsupported: misleading receipt text"),
      );
      const mounted = render(<NetworkTools {...props} />);
      await flush();
      if (batch) {
        fireEvent.click(screen.getByRole("button", { name: "诊断" }));
        await flush();
        fireEvent.click(screen.getByRole("button", { name: "运行完整诊断" }));
      } else {
        await toolbox();
        fireEvent.click(screen.getByRole("button", { name: "运行" }));
      }
      await flush();
      await advance(1000);
      expect(API.request).toHaveBeenCalledTimes(1);
      const id = (vi.mocked(API.request).mock.calls[0][1].data as any)
        .operation_id;
      const reads = vi
        .mocked(API.query)
        .mock.calls.filter((call) => call[1] === "network_report");
      if (rejected) {
        expect(
          sessionStorage.getItem("astra-network-job:agent-a:proxy"),
        ).toBeNull();
        expect(reads).toHaveLength(0);
      } else {
        expect(sessionStorage.getItem("astra-network-job:agent-a:proxy")).toBe(
          id,
        );
        expect(reads.length).toBeGreaterThan(0);
        expect(
          reads.every((call) => (call[2] as API.Data).operation_id === id),
        ).toBe(true);
      }
      mounted.unmount();
    }
  },
);

it.each([
  ["boot", "核心已重启"],
  ["digest", "已应用配置已变化"],
  ["runtime", "运行模式或代理选择已变化"],
])(
  "marks a path stale after %s changes without altering the submitted egress",
  async (change, label) => {
    render(<NetworkTools {...props} />);
    await flush();
    await toolbox();
    fireEvent.change(screen.getByLabelText("本次探测出口"), {
      target: { value: "iface:agent-a-nic" },
    });
    fireEvent.change(screen.getByLabelText("目标"), {
      target: { value: "192.0.2.10:443" },
    });
    fireEvent.click(screen.getByRole("button", { name: "运行" }));
    await flush();
    if (change === "boot") coreReply.identity!.boot_id = "core-boot-2";
    if (change === "digest") coreReply.identity!.applied_digest = "digest-2";
    if (change === "runtime") runtimeSnapshot.system_proxy = true;
    await advance(5000);
    expect(
      screen.getByText(new RegExp(`路径快照已过时：${label}`)),
    ).toBeTruthy();
    expect(
      screen.getByText("本次探测选择：目标 Agent 网卡 agent-a-nic"),
    ).toBeTruthy();
    expect(API.request).toHaveBeenCalledTimes(1);
    fireEvent.click(screen.getByRole("button", { name: "路径" }));
    expect(screen.getByText("目标 Agent 网卡 agent-a-nic")).toBeTruthy();
    expect(
      screen.getByText(new RegExp(`路径快照已过时：${label}`)),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /显示原始报告/ }));
    expect(
      screen.getByText("探测时核心启动标识").nextElementSibling?.textContent,
    ).toBe("core-boot-1");
    expect(
      screen.getByText("探测时配置摘要").nextElementSibling?.textContent,
    ).toBe("digest-1");
  },
);

it("does not treat changing traffic counters as a changed path", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  runtimeSnapshot.upload = "99999";
  runtimeSnapshot.active = 42;
  await advance(5000);
  expect(screen.queryByText(/路径快照已过时/)).toBeNull();
});

it("submits a configured outbound and optional resolver only after Run", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  expect(screen.getByRole("option", { name: "DIRECT" })).toBeTruthy();
  fireEvent.change(screen.getByLabelText("本次探测出口"), {
    target: { value: "outbound:proxy-node" },
  });
  fireEvent.change(screen.getByLabelText("解析器（可选）"), {
    target: { value: " tls://dns.example#bootstrap=192.0.2.53 " },
  });
  expect(API.request).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(vi.mocked(API.request).mock.calls[0][1]).toMatchObject({
    type: "execute",
    data: {
      command: {
        type: "network_diagnostics",
        data: {
          instance: "proxy",
          request: {
            egress: { kind: "outbound", name: "proxy-node" },
            resolver: "tls://dns.example#bootstrap=192.0.2.53",
          },
        },
      },
    },
  });
});

it("does not execute a resolver request when the Agent only declares jobs_v1", async () => {
  capabilities = ["network_diagnostics_jobs_v1"];
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.change(screen.getByLabelText("解析器（可选）"), {
    target: { value: "tcp://192.0.2.53" },
  });
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(screen.getByRole("alert").textContent).toContain("未声明新版解析能力");
  expect(API.request).not.toHaveBeenCalled();
  expect(sessionStorage.getItem("astra-network-job:agent-a:proxy")).toBeNull();
});

it.each(["tls", "dot", "https", "interface_tcp_name", "interface_dns_name"])(
  "gates the new %s resolution path on resolution_v1",
  async (path) => {
    capabilities = capabilities.filter(
      (capability) => capability !== "network_diagnostics_resolution_v1",
    );
    render(<NetworkTools {...props} />);
    await flush();
    await toolbox();
    if (path.startsWith("interface"))
      fireEvent.change(screen.getByLabelText("本次探测出口"), {
        target: { value: "iface:agent-a-nic" },
      });
    if (path !== "interface_tcp_name") {
      fireEvent.change(screen.getByLabelText("工具"), {
        target: { value: "dns" },
      });
      fireEvent.change(screen.getByLabelText("DNS 解析器"), {
        target: {
          value:
            path === "interface_dns_name"
              ? "tcp://resolver.example:53"
              : `${path}://192.0.2.53`,
        },
      });
    }
    fireEvent.click(screen.getByRole("button", { name: "运行" }));
    await flush();
    expect(screen.getByRole("alert").textContent).toContain(
      "未声明新版解析能力",
    );
    expect(API.request).not.toHaveBeenCalled();
  },
);

it("rechecks the latest Hello before submitting a pinned outbound", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.change(screen.getByLabelText("本次探测出口"), {
    target: { value: "outbound:proxy-node" },
  });
  capabilities = capabilities.filter(
    (capability) => capability !== "network_diagnostics_pinned_v1",
  );
  const reads = vi.mocked(API.query).mock.calls.length;
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(vi.mocked(API.query).mock.calls[reads].slice(0, 2)).toEqual([
    "agent-a",
    "hello",
  ]);
  expect(screen.getByRole("alert").textContent).toContain("未声明固定路径能力");
  expect(API.request).not.toHaveBeenCalled();
});

it("never drops a supplied resolver when the latest Hello only offers the legacy tool", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.change(screen.getByLabelText("解析器（可选）"), {
    target: { value: "tcp://192.0.2.53" },
  });
  capabilities = [];
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(screen.getByRole("alert").textContent).toContain("不会降级丢弃选项");
  expect(API.request).not.toHaveBeenCalled();
});

it("keeps basic IP-bound probes compatible with an older job-capable Agent", async () => {
  capabilities = capabilities.filter(
    (capability) =>
      ![
        "network_diagnostics_resolution_v1",
        "network_diagnostics_pinned_v1",
      ].includes(capability),
  );
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  fireEvent.change(screen.getByLabelText("本次探测出口"), {
    target: { value: "iface:agent-a-nic" },
  });
  fireEvent.change(screen.getByLabelText("目标"), {
    target: { value: "192.0.2.1:443" },
  });
  fireEvent.click(screen.getByRole("button", { name: "运行" }));
  await flush();
  expect(API.request).toHaveBeenCalledTimes(1);
  expect(vi.mocked(API.request).mock.calls[0][1]).toMatchObject({
    type: "execute",
    data: { command: { type: "network_diagnostics" } },
  });
});

it("rechecks Core identity before each manual Fake-IP query and leaves misses unclassified", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  expect(
    vi
      .mocked(API.query)
      .mock.calls.some((call) => call[1] === "fake_ip_lookup"),
  ).toBe(false);
  fireEvent.change(screen.getByLabelText("待查询 IP 地址"), {
    target: { value: "198.18.0.1, 2001:db8::1" },
  });
  const start = vi.mocked(API.query).mock.calls.length;
  fireEvent.click(screen.getByRole("button", { name: "查询 Fake-IP 映射" }));
  await flush();
  const calls = vi.mocked(API.query).mock.calls.slice(start);
  expect(calls.map((call) => [call[0], call[1], call[2]])).toEqual([
    ["agent-a", "network_core", { instance: "proxy" }],
    [
      "agent-a",
      "fake_ip_lookup",
      { instance: "proxy", addresses: ["198.18.0.1", "2001:db8::1"] },
    ],
  ]);
  expect(
    screen.getAllByText("未命中（不能据此判断是否为 Fake-IP）"),
  ).toHaveLength(2);
  expect(API.request).not.toHaveBeenCalled();
  coreReply.identity!.capabilities = [];
  fireEvent.click(screen.getByRole("button", { name: "查询 Fake-IP 映射" }));
  await flush();
  expect(screen.getByText(/Fake-IP 查询结果：明确不支持/)).toBeTruthy();
  expect(
    vi
      .mocked(API.query)
      .mock.calls.filter((call) => call[1] === "fake_ip_lookup"),
  ).toHaveLength(1);
});

it.each([
  ["unconfirmed", "能力无法确认（能力列表缺失）"],
  ["unsupported", "明确不支持"],
  ["communication_error", "通信失败"],
  ["protocol_error", "协议错误"],
] as const)(
  "shows %s in both overview and path without sending a feature query",
  async (status, label) => {
    coreReply.status = status;
    coreReply.identity = null;
    render(<NetworkTools {...props} />);
    await flush();
    expect(screen.getByText(`核心状态：${label}`)).toBeTruthy();
    fireEvent.change(screen.getByLabelText("待查询 IP 地址"), {
      target: { value: "198.18.0.1" },
    });
    fireEvent.click(screen.getByRole("button", { name: "查询 Fake-IP 映射" }));
    await flush();
    expect(
      screen.getByText(
        new RegExp(
          `Fake-IP 查询结果：${label.replaceAll("（", "\\（").replaceAll("）", "\\）")}`,
        ),
      ),
    ).toBeTruthy();
    expect(
      vi
        .mocked(API.query)
        .mock.calls.some((call) => call[1] === "fake_ip_lookup"),
    ).toBe(false);
    fireEvent.click(screen.getByRole("button", { name: "路径" }));
    expect(screen.getByText(`核心状态：${label}`)).toBeTruthy();
  },
);

it.each([
  Array.from({ length: 65 }, () => "198.18.0.1").join("\n"),
  "example.com",
  "256.1.1.1",
  "1.2.3",
  "2001:db8:::1",
  "[::1]",
  "fe80::1%en0",
])(
  "rejects invalid or excessive Fake-IP input before contacting Core: %s",
  async (input) => {
    render(<NetworkTools {...props} />);
    await flush();
    const reads = vi.mocked(API.query).mock.calls.length;
    fireEvent.change(screen.getByLabelText("待查询 IP 地址"), {
      target: { value: input },
    });
    fireEvent.click(screen.getByRole("button", { name: "查询 Fake-IP 映射" }));
    await flush();
    expect(screen.getByText(/请填写 1–64 个有效/)).toBeTruthy();
    expect(API.query).toHaveBeenCalledTimes(reads);
  },
);

it("accepts 64 IPs and rejects a Core boot change during lookup", async () => {
  const original = vi.mocked(API.query).getMockImplementation()!;
  vi.mocked(API.query).mockImplementation(
    async (agent, query, data, signal) => {
      const reply = await original(agent, query, data, signal);
      if (query === "fake_ip_lookup")
        return {
          ...reply,
          data: {
            ...(reply.data as Record<string, unknown>),
            boot_id: "different-core",
          },
        };
      return reply;
    },
  );
  render(<NetworkTools {...props} />);
  await flush();
  fireEvent.change(screen.getByLabelText("待查询 IP 地址"), {
    target: {
      value: Array.from({ length: 64 }, (_, i) => `198.18.0.${i}`).join(" "),
    },
  });
  fireEvent.click(screen.getByRole("button", { name: "查询 Fake-IP 映射" }));
  await flush();
  expect(
    vi
      .mocked(API.query)
      .mock.calls.filter((call) => call[1] === "fake_ip_lookup"),
  ).toHaveLength(1);
  expect(screen.getByText(/Fake-IP 查询结果：协议错误/)).toBeTruthy();
  expect(screen.queryByRole("region", { name: "Fake-IP 映射结果" })).toBeNull();
});

it("ignores a late Fake-IP result when switching the target Agent", async () => {
  let resolve!: (value: { type: string; data: unknown }) => void;
  const original = vi.mocked(API.query).getMockImplementation()!;
  vi.mocked(API.query).mockImplementation(
    async (agent, query, data, signal) => {
      if (query === "fake_ip_lookup")
        return new Promise((done) => {
          resolve = done;
        });
      return original(agent, query, data, signal);
    },
  );
  const { rerender } = render(<NetworkTools {...props} />);
  await flush();
  fireEvent.change(screen.getByLabelText("待查询 IP 地址"), {
    target: { value: "198.18.0.1" },
  });
  fireEvent.click(screen.getByRole("button", { name: "查询 Fake-IP 映射" }));
  await flush();
  rerender(<NetworkTools {...props} agent="agent-b" />);
  await flush();
  await act(async () =>
    resolve({
      type: "fake_ip_lookup",
      data: {
        schema_version: 1,
        instance: "proxy",
        sampled_at_ms: "2000",
        status: "ok",
        boot_id: "core-boot-1",
        mappings: [{ address: "198.18.0.1", domain: "old-agent.example" }],
        message: null,
      },
    }),
  );
  expect(
    (screen.getByLabelText("待查询 IP 地址") as HTMLTextAreaElement).value,
  ).toBe("");
  expect(screen.queryByText("old-agent.example")).toBeNull();
  expect(
    vi
      .mocked(API.query)
      .mock.calls.filter(
        (call) => call[0] === "agent-b" && call[1] === "fake_ip_lookup",
      ),
  ).toHaveLength(0);
});

it("restores a stopped task using its existing ID and recovered path evidence without replay", async () => {
  sessionStorage.setItem("astra-network-job:agent-a:proxy", "recover-job");
  const original = vi.mocked(API.query).getMockImplementation()!;
  vi.mocked(API.query).mockImplementation(
    async (agent, query, data, signal) => {
      if (query === "network_report")
        return {
          type: query,
          data: {
            ...report("recover-job"),
            cursor: "1",
            items: [
              {
                id: "1",
                sampled_at_ms: "1000",
                data: {
                  event: "context",
                  actual_egress: { kind: "outbound", name: "frozen-node" },
                  running_core: { boot_id: "probe-boot" },
                  config_digest: "probe-digest",
                  config_observation: "matched_running",
                  effective_interface: "en0",
                  interface_source: "outer_group",
                  egress_source: "pinned",
                  selection_chain: [
                    {
                      group: "outer",
                      mode: "select",
                      selected: "frozen-node",
                      source: "runtime",
                      interface: "en0",
                    },
                  ],
                },
              },
            ],
          },
        };
      return original(agent, query, data, signal);
    },
  );
  render(<NetworkTools {...props} />);
  await flush();
  await toolbox();
  expect(screen.getByText("本次探测选择：配置出口 frozen-node")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "停止观察" }));
  await flush();
  expect(
    (screen.getByLabelText("重连已有任务") as HTMLInputElement).value,
  ).toBe("recover-job");
  fireEvent.click(screen.getByRole("button", { name: "查询报告" }));
  await flush();
  expect(screen.getByText("本次探测选择：配置出口 frozen-node")).toBeTruthy();
  expect(API.request).not.toHaveBeenCalled();
  expect(sessionStorage.getItem("astra-network-job:agent-a:proxy")).toBe(
    "recover-job",
  );
  fireEvent.click(screen.getByRole("button", { name: "路径" }));
  expect(screen.queryByRole("region", { name: "本次固定选择链" })).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: /显示原始报告/ }));
  const chain = screen.getByRole("region", { name: "本次固定选择链" });
  expect(within(chain).getByText("outer")).toBeTruthy();
  expect(within(chain).getByText("frozen-node")).toBeTruthy();
  expect(within(chain).getByText("runtime")).toBeTruthy();
  expect(within(chain).getByText("en0")).toBeTruthy();
});

it.each([
  ["business_target", "业务目标"],
  ["proxy_entry", "代理入口"],
  ["dns_service", "DNS 服务名"],
])("labels %s resolution independently from DNS queries", (purpose, label) => {
  render(
    <DiagnosticResult
      advanced
      item={{
        id: "resolve-1",
        sampled_at_ms: "1000",
        data: {
          event: "resolution",
          seq: "2",
          purpose,
          host: "target.example",
          method: "specified_resolver",
          resolver: "tcp://192.0.2.53",
          addresses: ["192.0.2.20"],
          interface: "en0",
        },
      }}
    />,
  );
  expect(screen.getByText(`解析 #2 · ${label}`)).toBeTruthy();
  expect(screen.getByText("192.0.2.20")).toBeTruthy();
  expect(screen.getByText("tcp://192.0.2.53")).toBeTruthy();
  expect(screen.getByText("en0")).toBeTruthy();
});

it.each([
  "backward",
  "duplicate",
  "out-of-order",
  "empty-expired",
  "oversized-page",
  "oversized-report",
])(
  "rejects %s evidence without advancing or resubmitting the operation",
  async (failure) => {
    sessionStorage.setItem("astra-network-job:agent-a:proxy", "bounded-job");
    const original = vi.mocked(API.query).getMockImplementation()!;
    let pages = 0;
    vi.mocked(API.query).mockImplementation(
      async (agent, query, data, signal) => {
        if (query !== "network_report")
          return original(agent, query, data, signal);
        pages++;
        const page = report("bounded-job");
        page.has_more = true;
        page.cursor = String(pages);
        page.items = [
          {
            id: String(pages),
            sampled_at_ms: "1000",
            data: { event: "attempt", detail: "accepted evidence" },
          },
        ];
        if (failure === "oversized-report") {
          page.items[0].data = {
            event: "attempt",
            detail: "x".repeat(60 * 1024),
          };
        } else if (pages > 1) {
          if (failure === "backward") {
            page.cursor = "0";
            page.items = [];
          }
          if (failure === "duplicate") page.items[0].id = "1";
          if (failure === "out-of-order") {
            page.cursor = "3";
            page.items = [
              { id: "3", sampled_at_ms: "1000", data: { event: "attempt" } },
              { id: "2", sampled_at_ms: "1000", data: { event: "attempt" } },
            ];
          }
          if (failure === "empty-expired") {
            page.expired = true;
            page.items = [];
          }
          if (failure === "oversized-page")
            page.items[0].data = {
              event: "attempt",
              detail: "x".repeat(64 * 1024),
            };
        }
        return { type: "network_report", data: page };
      },
    );
    render(<NetworkTools {...props} />);
    await flush();
    await toolbox();
    expect(screen.getByText(/报告尚未确认/)).toBeTruthy();
    expect(API.request).not.toHaveBeenCalled();
    expect(pages).toBeLessThanOrEqual(35);
    const lastCursor = vi
      .mocked(API.query)
      .mock.calls.filter((call) => call[1] === "network_report")
      .at(-1)?.[2];
    expect((lastCursor as API.Data).cursor).toBe(
      failure === "oversized-report" ? "34" : "1",
    );
    expect(sessionStorage.getItem("astra-network-job:agent-a:proxy")).toBe(
      "bounded-job",
    );
  },
);

it("defaults to conclusions, toggles advanced evidence without network actions, and leaves editor keys alone", async () => {
  render(<NetworkTools {...props} />);
  await flush();
  expect(
    screen
      .getByRole("button", { name: /显示原始报告/ })
      .getAttribute("aria-expanded"),
  ).toBe("false");
  expect(screen.queryByText("核心启动标识")).toBeNull();
  expect(screen.queryByText("系统 DNS 与分域策略")).toBeNull();
  const count = vi.mocked(API.query).mock.calls.length;
  const toggle = screen.getByRole("button", { name: /显示原始报告/ });
  fireEvent.keyDown(toggle, { key: "d", ctrlKey: true });
  expect(screen.getByText("核心启动标识")).toBeTruthy();
  expect(screen.getByText("系统 DNS 与分域策略")).toBeTruthy();
  expect(vi.mocked(API.query).mock.calls.length).toBe(count);
  expect(API.request).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: /隐藏原始报告/ }));
  const editor = screen.getByLabelText("待查询 IP 地址");
  expect(fireEvent.keyDown(editor, { key: "d", ctrlKey: true })).toBe(true);
  expect(screen.getByRole("button", { name: /显示原始报告/ })).toBeTruthy();
  expect(fireEvent.keyDown(toggle, { key: "F8" })).toBe(true);
  expect(screen.getByRole("button", { name: /显示原始报告/ })).toBeTruthy();
});

it("keeps a colored TLS failure conclusion visible while hiding certificate evidence by default", () => {
  const item = {
    id: "tls-failure",
    sampled_at_ms: "1000",
    data: {
      event: "tls",
      detail: {
        kind: "handshake",
        verified: false,
        verification_attempted: true,
        verification_error: "UnknownIssuer",
        server_name: "example.com",
        received_certificates: 1,
      },
    },
  };
  const { rerender } = render(<DiagnosticResult item={item} />);
  expect(screen.getByText(/验证或握手失败/).className).toContain(
    "network-tone-error",
  );
  expect(screen.getByText("失败")).toBeTruthy();
  expect(screen.queryByText("完整 TLS 验证")).toBeNull();
  expect(screen.queryByText("原始证据")).toBeNull();
  rerender(<DiagnosticResult item={item} advanced />);
  expect(screen.getByText("完整 TLS 验证")).toBeTruthy();
  expect(screen.getByText("原始证据")).toBeTruthy();
});

it.each([
  ["symmetric_observed", "对称型"],
  ["full_cone_observed", "完全锥型"],
  ["restricted_cone_observed", "受限锥型"],
  ["port_restricted_cone_observed", "端口受限锥型"],
  ["no_translation_observed", "未观察到地址转换"],
])("shows supported NAT type %s as a readable conclusion", (type, label) => {
  render(
    <DiagnosticResult
      item={{
        id: "nat",
        sampled_at_ms: "1000",
        data: {
          event: "nat",
          kind: "report",
          report: {
            traditional_type: type,
            advanced_capable: true,
            partial: false,
            unstable: false,
            physical_path: { binding_verified: true, route_verified: true },
          },
        },
      }}
    />,
  );
  expect(
    screen.getByText(new RegExp(`NAT 类型：${label}`)).className,
  ).toContain("network-tone-info");
  expect(screen.queryByText("映射行为")).toBeNull();
});

it.each([
  { partial: true },
  { unstable: true },
  { advanced_capable: false },
  { traditional_type: null },
  { physical_path: { binding_verified: false, route_verified: true } },
])("withholds NAT type when the report does not support it (%j)", (patch) => {
  render(
    <DiagnosticResult
      item={{
        id: "nat",
        sampled_at_ms: "1000",
        data: {
          event: "nat",
          kind: "report",
          report: {
            traditional_type: "symmetric_observed",
            advanced_capable: true,
            partial: false,
            unstable: false,
            physical_path: { binding_verified: true, route_verified: true },
            ...patch,
          },
        },
      }}
    />,
  );
  expect(screen.getByText(/NAT 类型：未能确定/).className).toContain(
    "network-tone-warning",
  );
  expect(screen.queryByText(/NAT 类型：对称型/)).toBeNull();
});

it("keeps TUN-local uncertainty visible without displaying a misleading latency", () => {
  render(
    <DiagnosticResult
      item={{
        id: "echo",
        sampled_at_ms: "1000",
        data: {
          event: "echo",
          observation: {
            target: "example.com",
            tun_local: true,
            proves_reachability: false,
            rtt_ms: 0.1,
          },
        },
      }}
    />,
  );
  expect(screen.getByText(/不能确认目标可达/).className).toContain(
    "network-tone-warning",
  );
  expect(screen.queryByText(/0\.1/)).toBeNull();
});

it("keeps long DNS values concise while retaining full advanced evidence", () => {
  const full = "long-value-".repeat(100);
  const item = {
    id: "dns",
    sampled_at_ms: "1000",
    data: {
      event: "dns",
      detail: {
        kind: "records",
        section: "answer",
        records: [{ name: "example.com", record_type: "TXT", text: full }],
      },
    },
  };
  const { rerender } = render(<DiagnosticResult item={item} />);
  expect(
    screen.getByText(/完整内容见原始报告/).textContent!.length,
  ).toBeLessThan(240);
  expect(screen.queryByText(full)).toBeNull();
  rerender(<DiagnosticResult item={item} advanced />);
  expect(screen.getByText(full)).toBeTruthy();
});

it.each([
  [{ event: "tcp", result: { outcome: "connected" } }, "success"],
  [{ event: "tcp", result: { outcome: "timed_out" } }, "warning"],
  [{ event: "tcp", result: { outcome: "unreachable" } }, "error"],
  [{ event: "http", detail: { kind: "response", status: 404 } }, "error"],
  [{ event: "http", detail: { kind: "response", status: 503 } }, "error"],
  [
    {
      event: "dns",
      detail: { kind: "response", parsed: true, outcome: "resolved" },
    },
    "success",
  ],
  [
    {
      event: "dns",
      detail: { kind: "response", parsed: true, outcome: "nxdomain" },
    },
    "warning",
  ],
  [
    {
      event: "dns",
      detail: { kind: "response", parsed: true, outcome: "no_records" },
    },
    "warning",
  ],
  [
    {
      event: "dns",
      detail: { kind: "response", parsed: true, outcome: "server_failure" },
    },
    "error",
  ],
  [
    {
      event: "tls",
      detail: { kind: "trust", load_errors: ["unreadable certificate"] },
    },
    "warning",
  ],
  [
    { event: "tls", detail: { kind: "certificate", metadata_truncated: true } },
    "warning",
  ],
])("colors recorded outcome %j as %s", (event, tone) => {
  expect(conclusions(event)[0].tone).toBe(tone);
});

it("shows only the latest status per project, path and round while preserving all source evidence", () => {
  const items = [
    {
      data: {
        event: "project_status",
        project_id: "tcp",
        path_id: "p",
        round: 1,
        state: "running",
      },
    },
    {
      data: {
        event: "project_status",
        project_id: "tcp",
        path_id: "p",
        round: 2,
        state: "running",
      },
    },
    {
      data: {
        event: "project_status",
        project_id: "tcp",
        path_id: "p",
        round: 1,
        state: "failed",
      },
    },
    {
      data: {
        event: "project_status",
        project_id: "dns",
        path_id: "p",
        round: 1,
        state: "complete",
      },
    },
  ];
  expect(latestConclusions(items)).toEqual(items.slice(1));
  expect(items).toHaveLength(4);
});

it.each([
  { sent: 0, received: 0 },
  { sent: 2, received: 2 },
  { sent: 4, received: 0 },
])(
  "keeps aggregate packet counts neutral because they do not determine NAT type or trace success (%j)",
  (counts) => {
    expect(conclusions({ event: "summary", ...counts })[0].tone).toBe("info");
  },
);

it("keeps STUN server rejection visible even when a response was received", () => {
  render(
    <DiagnosticResult
      item={{
        id: "stun",
        sampled_at_ms: "1000",
        data: {
          event: "stun",
          observation: {
            response_received: true,
            mapped_endpoint: null,
            error: "server rejected request",
          },
        },
      }}
    />,
  );
  expect(
    screen.getByText(/STUN 失败：server rejected request/).className,
  ).toContain("network-tone-error");
});

it("does not treat a STUN response without a mapping as a confirmed exit", () => {
  expect(
    conclusions({
      event: "stun",
      observation: { response_received: true, mapped_endpoint: null },
    })[0].tone,
  ).toBe("warning");
});

it("opens the original report beside persistent conclusions and keeps superseded statuses only in the report", () => {
  const items: NetworkResult[] = [
    {
      id: "1",
      sampled_at_ms: "1000",
      data: {
        event: "project_status",
        project_id: "tcp",
        path_id: "p",
        round: 1,
        state: "running",
      },
    },
    {
      id: "2",
      sampled_at_ms: "1001",
      data: {
        event: "project_status",
        project_id: "tcp",
        path_id: "p",
        round: 1,
        state: "failed",
        message: "connection refused",
      },
    },
    {
      id: "3",
      sampled_at_ms: "1002",
      data: {
        event: "tcp",
        result: {
          host: "example.com",
          port: 443,
          outcome: "refused",
          message: "server rejected",
        },
      },
    },
  ];
  const { rerender } = render(
    <DiagnosticColumns items={items} advanced={false} />,
  );
  const left = screen.getByRole("region", { name: "结论" });
  expect(
    within(left).getByText(/TCP example.com:443 · 连接被拒绝/),
  ).toBeTruthy();
  expect(within(left).queryByText(/运行中/)).toBeNull();
  expect(screen.queryByRole("region", { name: "原始报告" })).toBeNull();
  rerender(<DiagnosticColumns items={items} advanced />);
  expect(screen.getByRole("region", { name: "结论" })).toBe(left);
  expect(
    within(left).getByText(/TCP example.com:443 · 连接被拒绝/),
  ).toBeTruthy();
  expect(within(left).queryByText(/运行中/)).toBeNull();
  const right = screen.getByRole("region", { name: "原始报告" });
  expect(within(right).getByText("running")).toBeTruthy();
  expect(within(right).getByText("server rejected")).toBeTruthy();
  expect(within(right).getAllByText("原始证据")).toHaveLength(3);
  rerender(<DiagnosticColumns items={items} advanced={false} />);
  expect(
    within(left).getByText(/TCP example.com:443 · 连接被拒绝/),
  ).toBeTruthy();
  expect(screen.queryByRole("region", { name: "原始报告" })).toBeNull();
});
