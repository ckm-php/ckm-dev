import { defineConfig, devices } from '@playwright/test';

const baseURL = 'http://127.0.0.1:14201';

export default defineConfig({
  testDir: 'ui/e2e',
  reporter: 'list',
  use: {
    baseURL,
    trace: 'retain-on-failure',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
  webServer: {
    command: 'pnpm dev:ui-test',
    url: baseURL,
    reuseExistingServer: false,
  },
});
