import type { Bridge } from './index';

export const mockBridge: Bridge = {
  appInfo: () => Promise.resolve({ name: 'CK Dev', version: '0.0.0-mock' }),
};
