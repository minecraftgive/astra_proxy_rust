import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: './tests/e2e',
  testMatch: 'monitor.spec.ts',
  workers: 1,
  timeout: 30_000,
  use: { baseURL: 'http://127.0.0.1:4174', viewport: { width: 1440, height: 1000 } },
  webServer: {
    command: 'npm run dev -- --port 4174 --strictPort',
    url: 'http://127.0.0.1:4174',
    reuseExistingServer: false,
  },
});
