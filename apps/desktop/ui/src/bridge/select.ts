export interface BridgeEnv {
  dev: boolean;
  mode: string;
  hasTauri: boolean;
}

/** The mock is only for Playwright journeys: Vite dev in `ui-test` mode, outside Tauri. */
export function shouldUseMock({ dev, mode, hasTauri }: BridgeEnv): boolean {
  return dev && mode === 'ui-test' && !hasTauri;
}
