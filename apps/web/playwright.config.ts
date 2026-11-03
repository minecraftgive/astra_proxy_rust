import {defineConfig} from '@playwright/test';
export default defineConfig({
  testDir:'./tests/e2e',fullyParallel:false,workers:1,timeout:90_000,
  outputDir:process.env.ASTRA_E2E_REPORT_DIR??'test-results',
  use:{baseURL:process.env.ASTRA_E2E_URL,ignoreHTTPSErrors:true,viewport:{width:1440,height:1000},screenshot:'only-on-failure',trace:'retain-on-failure'},
  reporter:[['list']],
});
