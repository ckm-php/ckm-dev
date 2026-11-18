import js from '@eslint/js';
import { defineConfig, globalIgnores } from 'eslint/config';
import reactHooks from 'eslint-plugin-react-hooks';
import tseslint from 'typescript-eslint';

export default defineConfig([
  // Flat-config ignores match from this folder, so nested outputs need `**/`.
  globalIgnores([
    '.claude/',
    'docs/',
    '**/target/',
    '**/dist/',
    'apps/desktop/src-tauri/gen/',
    '**/node_modules/',
    '**/playwright-report/',
    '**/test-results/',
    'pnpm-lock.yaml',
    'Cargo.lock',
  ]),
  {
    files: ['apps/desktop/**/*.{ts,tsx}'],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
    ],
  },
]);
