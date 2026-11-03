import { defineConfig } from '@playwright/test';

const localPort = process.env.ASTRA_E2E_PORT ?? '4193';
const localURL = `http://127.0.0.1:${localPort}`;

export default defineConfig({
  testDir: './tests/e2e',
  fullyParallel: false,
  workers: 1,
  timeout: 90_000,
  outputDir: process.env.ASTRA_E2E_REPORT_DIR ?? 'test-results',
  use: {
    baseURL: process.env.ASTRA_E2E_URL ?? localURL,
    ignoreHTTPSErrors: true,
    viewport: { width: 1440, height: 1000 },
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
  },
  // The monitor uses API fixtures; live management still requires its TLS URL
  // and credentials and retains its explicit skip when they are absent.
  webServer: process.env.ASTRA_E2E_URL ? undefined : {
    command: `npm run dev -- --port ${localPort} --strictPort`,
    url: localURL,
    reuseExistingServer: false,
  },
  reporter: [['list']],
});
