import { isTauri } from '@tauri-apps/api/core';
import { shouldUseMock } from './select';
import { tauriBridge } from './tauri';

export interface AppInfo {
  name: string;
  version: string;
}

export interface Bridge {
  appInfo(): Promise<AppInfo>;
}

export interface SelectedBridge {
  bridge: Bridge;
  isMock: boolean;
}

export async function getBridge(): Promise<SelectedBridge> {
  // The literal `import.meta.env.DEV` must stay in this condition: a production build replaces it
  // with `false`, which drops the branch and keeps the mock chunk out of the bundle.
  if (
    import.meta.env.DEV &&
    shouldUseMock({ dev: import.meta.env.DEV, mode: import.meta.env.MODE, hasTauri: isTauri() })
  ) {
    const { mockBridge } = await import('./mock');
    return { bridge: mockBridge, isMock: true };
  }
  return { bridge: tauriBridge, isMock: false };
}
