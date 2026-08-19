// Webview entry point (design doc §5.1, §13).
//
// Vanilla TypeScript plus lit-html: the consent screen must render instantly
// on weak hardware, so no React/Vue/Angular. The UI never decides anything —
// it renders what the Rust core reports and forwards the host's clicks back.

import { html, render } from 'lit-html';

import { consentDialog } from './consent-dialog';
import { detectLocale, dirOf, type Locale } from './i18n';
import { t } from './i18n';
import { connectPanel, inviteCodePanel, onInviteStateChange } from './invite-view';
import { logoMark } from './logo';
import { sessionStatus, type HistoryEntry, type SessionStatus } from './session-status';
import { statusPill } from './status-pill';
import { titleBar } from './title-bar';

const root = document.querySelector('#app');
let locale: Locale = detectLocale(navigator);

// Latest polled state (§ main.ts refresh loop below). Rendering is split
// from fetching so invite-view's own async UI state (copy feedback, connect
// errors) can trigger an immediate re-render without waiting on the next
// IPC round trip.
let sessions: SessionStatus[] = [];
let history: HistoryEntry[] = [];
let networkReady = false;

function applyDir(): void {
  document.documentElement.lang = locale;
  document.documentElement.dir = dirOf(locale);
}

function renderNow(): void {
  if (!root) {
    return;
  }
  applyDir();
  const pendingRequest = sessions.find((session) => session.state === 'pending');
  const activeSessions = sessions.filter((session) => session.state === 'active');

  render(
    [
      html`
        <div class="window-shell">
          ${titleBar(locale)}
          <div class="window-body">
            <aside class="sidebar">
              <div class="brand-row">${logoMark()}<span class="brand-name">Lumepeer</span></div>
              ${inviteCodePanel(locale)}
              <div class="sidebar-bottom">
                ${statusPill(networkReady, locale)}
                <div class="sidebar-divider"></div>
                <div class="footer-tag">
                  <svg width="12" height="12" viewBox="0 0 16 16" aria-hidden="true">
                    <circle cx="8" cy="8" r="6.5" stroke="currentColor" stroke-width="1.2" fill="none" />
                    <ellipse cx="8" cy="8" rx="3" ry="6.5" stroke="currentColor" stroke-width="1.2" fill="none" />
                    <line x1="1.5" y1="8" x2="14.5" y2="8" stroke="currentColor" stroke-width="1.2" />
                  </svg>
                  <span>${t(locale, 'sidebar.serverless')}</span>
                </div>
              </div>
            </aside>
            <main class="main-panel">
              ${connectPanel(locale)}
              <div class="main-divider"></div>
              ${sessionStatus(activeSessions, locale, () => void refresh(), history)}
            </main>
          </div>
        </div>
      `,
      consentDialog(pendingRequest, locale),
    ],
    root as HTMLElement,
  );
}

async function refresh(): Promise<void> {
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    const [sessionResult, historyResult, networkResult] = await Promise.all([
      invoke<SessionStatus[]>('session_status'),
      invoke<HistoryEntry[]>('connection_history'),
      invoke<{ ready: boolean }>('network_status'),
    ]);
    sessions = sessionResult;
    history = historyResult;
    networkReady = networkResult.ready;
  } catch (error) {
    console.error('refresh failed:', error);
  }
  renderNow();
}

// Exposed for manual/e2e locale switching; the consent screen itself carries
// no locale picker (§19 phase 6 doesn't ask for one, and adding UI chrome to
// a screen that must render instantly is scope creep) — the OS/webview
// locale via `navigator.language` is what `detectLocale` reads.
export function setLocale(next: Locale): void {
  locale = next;
  renderNow();
}

onInviteStateChange(renderNow);

renderNow();
void refresh();
setInterval(() => {
  void refresh();
}, 1000);
