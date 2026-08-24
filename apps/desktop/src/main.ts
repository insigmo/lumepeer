// Webview entry point (design doc §5.1, §13).
//
// Vanilla TypeScript plus lit-html: the consent screen must render instantly
// on weak hardware, so no React/Vue/Angular. The UI never decides anything —
// it renders what the Rust core reports and forwards the host's clicks back.

import { html, nothing, render, type TemplateResult } from 'lit-html';

import { ChatState, startChatPolling, tauriChatCommands } from './chat';
import { consentDialog } from './consent-dialog';
import { detectLocale, dirOf, type Locale } from './i18n';
import { t } from './i18n';
import {
  connectPanel,
  inviteCodePanel,
  isConnecting,
  onInviteStateChange,
  reconnect,
  setConnectPhase,
  type ConnectPhase,
} from './invite-view';
import { logoMark } from './logo';
import { sessionStatus, type HistoryEntry, type SessionStatus } from './session-status';
import { statusPill } from './status-pill';
import { titleBar } from './title-bar';

const root = document.querySelector('#app');
const chatPanel = document.querySelector<HTMLElement>('#host-chat-panel');
let locale: Locale = detectLocale(navigator);

// Latest polled state (§ main.ts refresh loop below). Rendering is split
// from fetching so invite-view's own async UI state (copy feedback, connect
// errors) can trigger an immediate re-render without waiting on the next
// IPC round trip.
let sessions: SessionStatus[] = [];
let history: HistoryEntry[] = [];
let networkReady = false;
// What this machine can do about producing a picture. Both true until
// `network_status` says otherwise, so a host that is fine shows no warning.
let canCapture = true;
let canEncode = true;

/**
 * Warns the operator that people they invite will see nothing.
 *
 * The session still connects and input still works, so this is a warning and
 * not an error state — but it has to be on this screen: without it the only
 * symptom is on the *guest's* screen, a minute later, as a connection failure
 * that never happened.
 */
function mediaWarning(): TemplateResult | typeof nothing {
  if (canCapture && canEncode) {
    return nothing;
  }
  const key = canCapture ? 'status.noEncoder' : 'status.noCapture';
  return html`<p class="media-warning" role="status" aria-live="polite">${t(locale, key)}</p>`;
}

// Host-side chat with one peer at a time. The panel is the same chat.ts
// component the view window mounts; only the placement differs. The poll's
// stop handle is kept so switching peers or closing the drawer does not leave
// the previous transcript polling behind.
let chatStop: (() => void) | undefined;
let chatPeer: string | undefined;

/** Opens (or re-targets) the host chat drawer onto `peer`. */
function openChat(peer: string): void {
  if (!chatPanel) {
    return;
  }
  if (chatPeer !== peer) {
    chatStop?.();
    // Header (title + close) plus the body the poll loop renders into; the
    // close button is re-created per peer so its handler never goes stale.
    chatPanel.replaceChildren();
    const head = document.createElement('div');
    head.className = 'host-chat-head';
    const body = document.createElement('div');
    body.className = 'host-chat-body';
    const close = document.createElement('button');
    close.type = 'button';
    close.className = 'host-chat-close';
    close.setAttribute('aria-label', t(locale, 'chat.close'));
    close.textContent = '×';
    close.addEventListener('click', closeChat);
    head.append(close);
    chatPanel.append(head, body);
    chatStop = startChatPolling(body, new ChatState(), locale, peer, tauriChatCommands);
    chatPeer = peer;
  }
  chatPanel.hidden = false;
}

/** Closes the host chat drawer and stops its transcript poll. */
function closeChat(): void {
  chatStop?.();
  chatStop = undefined;
  chatPeer = undefined;
  if (chatPanel) {
    chatPanel.hidden = true;
    render(html``, chatPanel);
  }
}

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
              ${mediaWarning()} ${connectPanel(locale)}
              <div class="main-divider"></div>
              ${sessionStatus(
                activeSessions,
                locale,
                () => void refresh(),
                history,
                (peer) => void reconnect(peer),
                isConnecting(),
                openChat,
              )}
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
    const [sessionResult, historyResult, networkResult, connectResult] = await Promise.all([
      invoke<SessionStatus[]>('session_status'),
      invoke<HistoryEntry[]>('connection_history'),
      invoke<{ ready: boolean; can_capture: boolean; can_encode: boolean }>('network_status'),
      invoke<{ phase: ConnectPhase; pending: boolean; code: string | null }>('connect_status'),
    ]);
    sessions = sessionResult;
    history = historyResult;
    networkReady = networkResult.ready;
    canCapture = networkResult.can_capture;
    canEncode = networkResult.can_encode;
    // The peer the drawer is open on ended its session: close rather than
    // leave a transcript poll spinning on a dead label (the poll itself stops
    // on the first IPC error, but this keeps the UI honest immediately).
    if (chatPeer && !sessions.some((s) => s.state === 'active' && s.peer_label === chatPeer)) {
      closeChat();
    }
    // The connect form's own wait: a dial that returned is not a session yet,
    // and only the actor knows whether the far side has decided (§21 item 6).
    setConnectPhase(connectResult.phase, connectResult.code);
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
