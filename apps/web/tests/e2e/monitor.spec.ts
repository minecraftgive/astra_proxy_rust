import { test, expect } from '@playwright/test';

// Deterministic management fixtures exercise the actual app, navigation and downloads.
// Native sampling and authorization are tested separately in the Rust crates.
test('monitor renders, pauses, exports and fits a narrow viewport', async ({ page }, testInfo) => {
  let samples = 0;
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/api/v2/login', route => route.fulfill({ json: { token: 'monitor-fixture' } }));
  await page.route('**/api/v2/events?*', route => route.fulfill({
    contentType: 'text/event-stream', body: ': heartbeat\n\n',
  }));
  await page.route('**/api/v2/request', async route => {
    const request = route.request().postDataJSON();
    const query = request.body.data;
    let data: unknown;
    switch (query.type) {
      case 'agents':
        data = [{ id: 'fixture', name: '本地调试节点', boot_id: 'fixture-boot', online: true, last_seen_ms: '1793664000000' }];
        break;
      case 'instances':
        data = [{ id: 'proxy', name: '开发代理', kind: 'proxy', config: 'proxy.toml', pid: 321,
          state: 'Running', uptime_secs: '120', capabilities: ['monitor'], changed: false, details: {} }];
        break;
      case 'runtime':
        data = { kind: 'client', snapshot: { pid: 321, mode: 'rule', upload: '1024', download: '4096', active: 4, total: '20', connections: [], groups: [] } };
        break;
      case 'monitor':
        samples++;
        data = { instance: 'proxy', pid: 321, sampled_at_ms: String(1793664000000 + samples * 1000),
          uptime_secs: String(120 + samples), cpu_percent: 12.5, memory_bytes: '67108864', virtual_memory_bytes: '268435456',
          upload_bytes: String(1024000 + samples * 1024), download_bytes: String(4096000 + samples * 4096),
          active_connections: 4, total_connections: '20', error: null,
          internal: { epoch: 'fixture-epoch', metrics: [
            { sampled_at_ms: String(1793664000000 + samples * 1000), name: 'routing.completed', value: String(samples * 10), kind: 'counter', unit: 'count', description: '路由决策：完成（含错误）' },
            { sampled_at_ms: String(1793664000000 + samples * 1000), name: 'routing.duration_us', value: String(samples * 25000), kind: 'counter', unit: 'us', description: '路由决策：累计完成耗时' },
            { sampled_at_ms: String(1793664000000 + samples * 1000), name: 'routing.in_flight', value: '2', kind: 'gauge', unit: 'count', description: '路由决策：正在处理' },
            { sampled_at_ms: String(1793664000000 + samples * 1000), name: 'dns.cache.hits', value: String(9007199254740993n + BigInt(samples)), kind: 'counter', unit: 'count', description: 'DNS 缓存命中' },
          ] } };
        break;
      case 'tasks': data = []; break;
      default: throw new Error(`Unexpected query: ${query.type}`);
    }
    await route.fulfill({ json: { version: 2, id: request.id, result: { Ok: { type: query.type, data } } } });
  });
  await page.goto('/');
  await page.getByLabel('管理员密码', { exact: true }).fill('fixture-password');
  await page.getByRole('button', { name: '登录', exact: true }).click();
  await page.getByRole('button', { name: /监视器/ }).click();
  await expect(page.locator('dd').filter({ hasText: /^4\.0 KiB\/s$/ })).toBeVisible();
  await expect(page.getByText('12.5%', { exact: true })).toBeVisible();
  await page.screenshot({ path: testInfo.outputPath('monitor-desktop.png'), fullPage: true });
  await page.getByRole('heading', { name: '程序内部统计' }).scrollIntoViewIfNeeded();
  await expect(page.getByText('DNS 缓存命中', { exact: false })).toBeVisible();
  await page.getByText('DNS 缓存命中', { exact: false }).scrollIntoViewIfNeeded();
  await page.screenshot({ path: testInfo.outputPath('monitor-internal.png'), fullPage: true });
  await page.getByRole('button', { name: /暂停采样/ }).click();
  const paused = samples;
  // Beyond the shortest selectable interval: pause must stop requests, not just rendering.
  await page.waitForTimeout(2200);
  expect(samples).toBe(paused);
  const download = page.waitForEvent('download');
  await page.getByRole('button', { name: /导出 JSON/ }).click();
  const file = await download;
  expect(file.suggestedFilename()).toMatch(/\.json$/);
  const stream = await file.createReadStream();
  const chunks: Buffer[] = [];
  for await (const chunk of stream!) chunks.push(Buffer.from(chunk));
  const exported = Buffer.concat(chunks).toString('utf8');
  expect(exported).toContain('67108864');
  expect(exported).toContain('dns.cache.hits');
  expect(exported).toContain('fixture-epoch');
  expect(exported).not.toContain('monitor-fixture');
  await page.setViewportSize({ width: 390, height: 844 });
  await page.screenshot({ path: testInfo.outputPath('monitor-mobile.png'), fullPage: true });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  expect((await page.locator('.monitor-controls').boundingBox())!.width).toBeGreaterThan(300);
  expect(errors).toEqual([]);
});
