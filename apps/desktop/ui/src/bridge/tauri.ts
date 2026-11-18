import { invoke } from '@tauri-apps/api/core';
import type { AppInfo, Bridge } from './index';

export const tauriBridge: Bridge = {
  appInfo: () => invoke<AppInfo>('app_info'),
};
