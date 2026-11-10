import { test, expect } from "@playwright/test";

test("network tools stay manual, show the link and fit desktop and narrow widths", async ({
  page,
}, testInfo) => {
  const executions: unknown[] = [];
  let fakeLookups = 0;
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await page.route("**/api/v2/login", (route) =>
    route.fulfill({ json: { token: "network-fixture" } }),
  );
  await page.route("**/api/v2/events?*", (route) =>
    route.fulfill({
      contentType: "text/event-stream",
      body: ": heartbeat\n\n",
    }),
  );
  await page.route("**/api/v2/request", async (route) => {
    const request = route.request().postDataJSON();
    const query = request.body.data;
    let type = query.type;
    let data: unknown;
    if (request.body.type === "execute") {
      executions.push(query);
      type = "task";
      data = { id: query.operation_id, state: "running" };
    } else
      switch (type) {
        case "agents":
          data = [
            {
              id: "fixture",
              name: "目标节点",
              boot_id: "agent-boot",
              online: true,
              last_seen_ms: "1793664000000",
            },
          ];
          break;
        case "instances":
          data = [
            {
              id: "proxy",
              name: "开发代理",
              kind: "proxy",
              config: "proxy.toml",
              pid: 42,
              state: "Running",
              uptime_secs: "120",
              capabilities: [],
              changed: false,
              details: {},
            },
          ];
          break;
        case "runtime":
          data = {
            kind: "client",
            snapshot: {
              pid: 42,
              mode: "rule",
              tun: true,
              system_proxy: false,
              groups: [
                {
                  name: "选择组",
                  selected: "固定节点",
                  mode: "select",
                  members: [],
                },
              ],
            },
          };
          break;
        case "node_tags":
          data = ["固定节点"];
          break;
        case "hello":
          data = {
            version: 2,
            role: "agent",
            epoch: "agent-boot",
            build_id: "fixture",
            capabilities: [
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
            ],
          };
          break;
        case "network_core":
          data = {
            schema_version: 1,
            instance: "proxy",
            sampled_at_ms: "1793664000000",
            status: "ok",
            message: null,
            identity: {
              version: 1,
              boot_id: "core-boot",
              pid: 42,
              applied_digest: "a".repeat(64),
              capabilities: ["fake_ip_lookup_v1"],
            },
          };
          break;
        case "network_state":
          data = {
            schema_version: 1,
            agent_id: "fixture",
            sampled_at_ms: "1793664000000",
            dns_sampled_at_ms: "1793664000000",
            interfaces: [
              {
                id: "en0",
                name: "目标节点物理网卡",
                index: 3,
                kind: "physical",
                connected: true,
                addresses: [
                  {
                    address: "2001:db8:1234:abcd:1234:abcd:1234:abcd",
                    prefix_len: 64,
                  },
                ],
                source: "fixture",
              },
            ],
            dns: [
              {
                id: "dns-1",
                source: "system_fixture",
                interface_id: null,
                scope: "policy",
                domains: [
                  "a-very-long-split-dns-domain.for-the-target-network.example",
                ],
                servers: ["2001:db8:1234:abcd:1234:abcd:1234:53"],
                priority: 100,
                flags: [],
              },
            ],
            routes: [
              {
                destination: "default",
                gateway: null,
                interface_id: "en0",
                flags: [],
                source: "fixture",
              },
            ],
            issues: [],
            truncated: false,
          };
          break;
        case "fake_ip_lookup":
          fakeLookups++;
          data = {
            schema_version: 1,
            instance: "proxy",
            sampled_at_ms: "1793664001000",
            status: "ok",
            message: null,
            boot_id: "core-boot",
            mappings: query.data.addresses.map((address: string) => ({
              address,
              domain: null,
            })),
          };
          break;
        case "network_report":
          data = {
            operation_id: query.data.operation_id,
            schema_version: 1,
            revision: "1",
            state: "complete",
            message: "Fixture complete",
            started_ms: "1793664000000",
            finished_ms: "1793664001000",
            truncated: false,
            expired: false,
            cursor: "7",
            has_more: false,
            items: [
              {
                id: "1",
                sampled_at_ms: "1793664001000",
                data: {
                  event: "dns",
                  seq: "1",
                  detail: {
                    kind: "records",
                    section: "answer",
                    records: [
                      {
                        name: "a-very-long-diagnostic-response-name.example.",
                        record_type: "TXT",
                        class: "IN",
                        ttl: "300",
                        text: "long-text-evidence-".repeat(40),
                        text_truncated: false,
                      },
                    ],
                  },
                },
              },
              ...Array.from({ length: 6 }, (_, index) => ({
                id: String(index + 2),
                sampled_at_ms: "1793664001000",
                data: {
                  event: "resolution",
                  seq: index + 1,
                  purpose: "business_target",
                  host: `target-${index}.example`,
                  method: "specified_resolver",
                  resolver: "udp://192.0.2.53",
                  addresses: ["192.0.2.80"],
                  interface: "physical-1",
                },
              })),
            ],
          };
          break;
        case "tasks":
          data = [];
          break;
        default:
          throw new Error(`Unexpected query ${type}`);
      }
    await route.fulfill({
      json: { version: 2, id: request.id, result: { Ok: { type, data } } },
    });
  });
  await page.goto("/");
  await page.getByLabel("管理员密码", { exact: true }).fill("fixture-password");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await page.getByRole("button", { name: /网络工具/ }).click();
  const chain = page.getByTestId("network-live-chain");
  await expect(chain).toContainText("网卡 目标节点物理网卡（默认路由）");
  expect(executions).toHaveLength(0);
  expect(fakeLookups).toBe(0);
  // A pinned outbound follows the instance's group selection.
  await page.getByRole("radio", { name: "TCP 连接" }).click();
  await page.getByLabel("出口", { exact: true }).selectOption("outbound:选择组");
  await expect(chain).toContainText("选择组 → 固定节点");
  await page.getByLabel("目标", { exact: true }).fill("example.com:80,443");
  await page.getByRole("button", { name: "运行", exact: true }).click();
  await expect(page.getByRole("log", { name: "结果" })).toContainText(
    "long-text-evidence-",
  );
  expect(executions).toHaveLength(1);
  expect(
    (executions[0] as any).command.data.request.probe,
  ).toEqual({ kind: "tcp_ports", host: "example.com", ports: [80, 443] });
  await page.screenshot({
    path: testInfo.outputPath("network-tools-desktop.png"),
    fullPage: true,
  });
  // Narrow width: no horizontal page scroll; long results scroll inside their box.
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(chain).toBeVisible();
  const overflow = await page.evaluate(
    () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
  );
  expect(overflow).toBeLessThanOrEqual(1);
  await page.screenshot({
    path: testInfo.outputPath("network-tools-narrow.png"),
    fullPage: true,
  });
  // Fake-IP stays a read-only lookup.
  await page.getByRole("radio", { name: "Fake-IP" }).click();
  await page.getByLabel("IP 地址").fill("198.18.0.5");
  await page.getByRole("button", { name: "查询映射" }).click();
  await expect(page.getByText("198.18.0.5 → 未查到映射，无法判断用途")).toBeVisible();
  expect(fakeLookups).toBe(1);
  expect(executions).toHaveLength(1);
  expect(errors).toEqual([]);
});
