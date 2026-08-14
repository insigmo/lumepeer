// Webview entry point (design doc §5.1, §13).
//
// Vanilla TypeScript plus lit-html: the consent screen must render instantly
// on weak hardware, so no React/Vue/Angular. The UI never decides anything —
// it renders what the Rust core reports and forwards the host's clicks back.

import { render } from 'lit-html';

import { consentDialog } from './consent-dialog';
import { detectLocale, dirOf, type Locale } from './i18n';
import { inviteView } from './invite-view';
import { sessionStatus, type SessionStatus } from './session-status';

const root = document.querySelector('#app');
let locale: Locale = detectLocale(navigator);

function applyDir(): void {
  document.documentElement.lang = locale;
  document.documentElement.dir = dirOf(locale);
}

async function refresh(): Promise<void> {
  if (!root) {
    return;
  }
  applyDir();
  const { invoke } = await import('@tauri-apps/api/core');
  const sessions = await invoke<SessionStatus[]>('session_status');
  const pendingRequest = sessions.find((session) => session.state === 'pending');
  const activeSessions = sessions.filter((session) => session.state === 'active');

  render(
    [inviteView(locale), consentDialog(pendingRequest, locale), sessionStatus(activeSessions, locale)],
    root as HTMLElement,
  );
}

// Exposed for manual/e2e locale switching; the consent screen itself carries
// no locale picker (§19 phase 6 doesn't ask for one, and adding UI chrome to
// a screen that must render instantly is scope creep) — the OS/webview
// locale via `navigator.language` is what `detectLocale` reads.
export function setLocale(next: Locale): void {
  locale = next;
  void refresh();
}

void refresh();
setInterval(() => {
  void refresh();
}, 1000);
