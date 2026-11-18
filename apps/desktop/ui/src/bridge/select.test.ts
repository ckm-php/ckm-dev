import { describe, expect, it } from 'vitest';
import { shouldUseMock } from './select';

describe('shouldUseMock', () => {
  it('never uses the mock in a production build', () => {
    expect(shouldUseMock({ dev: false, mode: 'ui-test', hasTauri: false })).toBe(false);
    expect(shouldUseMock({ dev: false, mode: 'production', hasTauri: false })).toBe(false);
  });

  it('does not use the mock in dev outside ui-test mode', () => {
    expect(shouldUseMock({ dev: true, mode: 'development', hasTauri: false })).toBe(false);
  });

  it('does not use the mock inside Tauri, even in ui-test mode', () => {
    expect(shouldUseMock({ dev: true, mode: 'ui-test', hasTauri: true })).toBe(false);
  });

  it('uses the mock in dev ui-test mode without Tauri', () => {
    expect(shouldUseMock({ dev: true, mode: 'ui-test', hasTauri: false })).toBe(true);
  });
});
