import { test, expect } from "@playwright/test";

test("network tools remain manual and usable at desktop and narrow widths", async ({
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
            routes: [],
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
  await expect(
    page.getByText("核心状态：能力已确认", { exact: true }),
  ).toBeVisible();
  expect(executions).toHaveLength(0);
  expect(fakeLookups).toBe(0);
  await expect(page.getByText("核心启动标识", { exact: true })).toHaveCount(0);
  const toggle = page.getByRole("button", { name: /显示原始报告/ });
  await toggle.focus();
  await page.keyboard.press("Control+d");
  await expect(page.getByText("核心启动标识", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: /隐藏原始报告/ }).click();
  await expect(page.getByText("核心启动标识", { exact: true })).toHaveCount(0);
  await page.screenshot({
    path: testInfo.outputPath("network-desktop.png"),
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.getByRole("button", { name: /显示原始报告/ }).click();
  const scroll = page.locator(".network-tools .table-scroll").first();
  expect(
    await scroll.evaluate(
      (element) => element.scrollWidth > element.clientWidth,
    ),
  ).toBe(true);
  await page.getByLabel("待查询 IP 地址").fill("198.18.0.1");
  await page
    .getByRole("button", { name: "查询 Fake-IP 映射", exact: true })
    .click();
  await expect(
    page.getByText("未命中（不能据此判断是否为 Fake-IP）", { exact: true }),
  ).toBeVisible();
  expect(fakeLookups).toBe(1);
  expect(executions).toHaveLength(0);
  await page
    .getByRole("region", { name: "Fake-IP 映射结果" })
    .scrollIntoViewIfNeeded();
  await page.screenshot({
    path: testInfo.outputPath("network-mobile-overview.png"),
    fullPage: true,
  });
  await page.getByRole("button", { name: /隐藏原始报告/ }).click();
  await page.getByRole("button", { name: "工具箱", exact: true }).click();
  await page.getByLabel("本次探测出口").selectOption("outbound:固定节点");
  await page
    .getByLabel("解析器（可选）")
    .fill(
      "https://resolver.example/private-path?token=fixture-secret#bootstrap=192.0.2.53",
    );
  await page.getByRole("button", { name: "运行", exact: true }).click();
  await expect(
    page.getByText("Fixture complete", { exact: true }),
  ).toBeVisible();
  expect(executions).toHaveLength(1);
  const pendingDownload = page.waitForEvent("download");
  await page.getByRole("button", { name: "导出 JSON", exact: true }).click();
  const download = await pendingDownload;
  const stream = await download.createReadStream();
  const chunks: Buffer[] = [];
  for await (const chunk of stream!) chunks.push(Buffer.from(chunk));
  const exported = Buffer.concat(chunks).toString("utf8");
  expect(exported).toContain('"report"');
  expect(exported).not.toContain("fixture-secret");
  expect(exported).not.toContain("private-path");
  await expect(
    page.getByText(/a-very-long-diagnostic-response-name.example.*TXT →/),
  ).toBeVisible();
  await expect(
    page.getByRole("region", { name: "DNS 记录", exact: true }),
  ).toHaveCount(0);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.locator(".network-result").first().scrollIntoViewIfNeeded();
  await page.screenshot({
    path: testInfo.outputPath("network-mobile-conclusions.png"),
    fullPage: true,
  });
  await page.getByRole("button", { name: /显示原始报告/ }).click();
  const conclusionColumn = page.getByRole("region", {
    name: "结论",
    exact: true,
  });
  const reportColumn = page.getByRole("region", {
    name: "原始报告",
    exact: true,
  });
  const resultScroll = page.getByRole("region", {
    name: "结论与原始报告",
    exact: true,
  });
  for (const width of [1440, 390]) {
    await page.setViewportSize({ width, height: width === 1440 ? 1000 : 844 });
    await conclusionColumn.scrollIntoViewIfNeeded();
    const left = (await conclusionColumn.boundingBox())!;
    const right = (await reportColumn.boundingBox())!;
    expect(right.x).toBeGreaterThanOrEqual(left.x + left.width);
    expect(Math.abs(right.y - left.y)).toBeLessThan(2);
    await expect(conclusionColumn.getByText(/TXT →/)).toBeVisible();
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
    if (width === 390)
      expect(
        await resultScroll.evaluate(
          (element) => element.scrollWidth > element.clientWidth,
        ),
      ).toBe(true);
    const conclusionBefore = await conclusionColumn.evaluate((element) => ({
      scroll: element.scrollTop,
      top: element.getBoundingClientRect().top,
    }));
    await reportColumn.evaluate((element) => {
      element.scrollTop = element.scrollHeight;
    });
    expect(
      await reportColumn.evaluate((element) => element.scrollTop),
    ).toBeGreaterThan(0);
    expect(
      await conclusionColumn.evaluate((element) => ({
        scroll: element.scrollTop,
        top: element.getBoundingClientRect().top,
      })),
    ).toEqual(conclusionBefore);
    await reportColumn.evaluate((element) => {
      element.scrollTop = 0;
    });
    await page.screenshot({
      path: testInfo.outputPath(`network-columns-${width}.png`),
      fullPage: true,
    });
  }
  await expect(
    page.getByRole("cell", { name: "TXT", exact: true }),
  ).toBeVisible();
  expect(
    (await page.locator(".network-record").boundingBox())!.height,
  ).toBeLessThanOrEqual(240);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await reportColumn
    .locator(".network-result")
    .first()
    .scrollIntoViewIfNeeded();
  await page.screenshot({
    path: testInfo.outputPath("network-mobile-results.png"),
    fullPage: true,
  });
  const records = page.getByRole("region", { name: "DNS 记录", exact: true });
  await records.evaluate((element) => {
    element.scrollLeft = element.scrollWidth;
  });
  expect(
    await records.evaluate((element) => element.scrollLeft),
  ).toBeGreaterThan(0);
  await page.screenshot({
    path: testInfo.outputPath("network-mobile-dns-records.png"),
    fullPage: true,
  });
  await page.getByRole("button", { name: "路径", exact: true }).click();
  await expect(
    page.getByText("配置出口 固定节点", { exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: testInfo.outputPath("network-mobile-path.png"),
    fullPage: true,
  });
  const sentBefore = executions.length;
  await page.getByRole("button", { name: "工具箱", exact: true }).click();
  await page
    .getByRole("combobox", { name: "工具", exact: true })
    .selectOption("nat");
  await expect(
    page.getByRole("combobox", { name: "NAT 物理网卡", exact: true }),
  ).toHaveValue("");
  await page.getByRole("button", { name: "运行", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("请选择物理网卡");
  await page
    .getByRole("combobox", { name: "地址族", exact: true })
    .selectOption("ipv6");
  await expect(
    page
      .getByRole("combobox", { name: "NAT 物理网卡", exact: true })
      .getByRole("option"),
  ).toHaveCount(2);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: testInfo.outputPath("network-mobile-nat.png"),
    fullPage: true,
  });
  await page.getByRole("button", { name: "诊断", exact: true }).click();
  await page.getByLabel("包含 NAT 检测", { exact: false }).check();
  await expect(
    page.getByRole("combobox", { name: "批量 NAT 物理网卡", exact: true }),
  ).toHaveValue("");
  await page.getByRole("button", { name: "添加路径", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "添加路径", exact: true }),
  ).toBeDisabled();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: testInfo.outputPath("network-mobile-batch.png"),
    fullPage: true,
  });
  expect(executions.length).toBe(sentBefore);
  expect(errors).toEqual([]);
});
