import { useEffect, useState } from 'react';
import type { AppInfo, Bridge } from './bridge';

type InfoState = { kind: 'loading' } | { kind: 'ready'; info: AppInfo } | { kind: 'error' };

interface AppProps {
  bridge: Bridge;
  isMock: boolean;
}

export function App({ bridge, isMock }: AppProps) {
  const [state, setState] = useState<InfoState>({ kind: 'loading' });

  useEffect(() => {
    let active = true;
    bridge.appInfo().then(
      (info) => {
        if (!active) return;
        document.title = info.name;
        setState({ kind: 'ready', info });
      },
      (reason: unknown) => {
        console.error('app_info failed', reason);
        if (active) setState({ kind: 'error' });
      },
    );
    return () => {
      active = false;
    };
  }, [bridge]);

  return (
    <main className="shell">
      {isMock && (
        <p className="mock-label" role="status">
          Mock data
        </p>
      )}
      {state.kind === 'loading' && <p className="muted">Loading…</p>}
      {state.kind === 'error' && (
        <p className="error" role="alert">
          Could not reach the app core.
        </p>
      )}
      {state.kind === 'ready' && (
        <>
          <h1>{state.info.name}</h1>
          <p className="muted">Version {state.info.version}</p>
        </>
      )}
    </main>
  );
}
