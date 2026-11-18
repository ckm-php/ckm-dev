import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

// `pnpm dev` (Tauri's devUrl) and `pnpm dev:ui-test` (Playwright) use separate fixed ports,
// so a running desktop dev session never serves the mock-bridge journeys.
const DEV_PORT = 14200;
const UI_TEST_PORT = 14201;

export default defineConfig(({ mode }) => ({
  root: 'ui',
  clearScreen: false,
  plugins: [react()],
  server: {
    host: '127.0.0.1',
    port: mode === 'ui-test' ? UI_TEST_PORT : DEV_PORT,
    strictPort: true,
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  test: {
    // Relative to `root`; keeps Vitest away from the Playwright specs in ui/e2e.
    include: ['src/**/*.test.ts'],
  },
}));
