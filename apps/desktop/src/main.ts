// Webview entry point (design doc §5.1, §13).
//
// Vanilla TypeScript plus lit-html: the consent screen must render instantly
// on weak hardware, so no React/Vue/Angular. The UI never decides anything —
// it renders what the Rust core reports and forwards the host's clicks back.

import { render } from 'lit-html';

import { consentDialog } from './consent-dialog';
import { sessionStatus, type SessionStatus } from './session-status';

const root = document.querySelector('#app');

async function refresh(): Promise<void> {
  if (!root) {
    return;
  }
  const { invoke } = await import('@tauri-apps/api/core');
  const sessions = await invoke<SessionStatus[]>('session_status');
  const pending = sessions.length === 0;

  render(
    [
      pending ? consentDialog(undefined) : consentDialog(sessions[0]),
      sessionStatus(sessions),
    ],
    root as HTMLElement,
  );
}

void refresh();
setInterval(() => {
  void refresh();
}, 1000);
