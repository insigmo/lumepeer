// Webview entry point (design doc §5.1, §13).
//
// Vanilla TypeScript plus lit-html: the consent screen must render instantly
// on weak hardware, so no React/Vue/Angular. The UI never decides anything —
// it renders what the Rust core reports and forwards the host's clicks back.

import { html, nothing, render, type TemplateResult } from 'lit-html';

import {
  addressBook,
  onAddressBookStateChange,
  saveDeviceButton,
  type AddressBookEntry,
} from './address-book';
import { ChatState, startChatPolling, tauriChatCommands } from './chat';
import { consentDialog } from './consent-dialog';
import { detectLocale, dirOf, type Locale } from './i18n';
import { t } from './i18n';
import {
  connectPanel,
  credentialsPanel,
  inviteCodePanel,
  isAwaitingCredentials,
  isConnecting,
  onInviteStateChange,
  reconnect,
  setConnectPhase,
  type ConnectPhase,
} from './invite-view';
import { logoMark } from './logo';
import {
  onRecordingsStateChange,
  recordingsPanel,
  tauriRecordingsCommands,
  type RecordingEntry,
} from './recordings';
import type { FileTransfers } from './file-transfers';
import { sessionStatus, type HistoryEntry, type SessionStatus } from './session-status';
import { statusPill } from './status-pill';
import { titleBar } from './title-bar';
import {
  onUnattendedStateChange,
  unattendedIndicator,
  unattendedSettings,
  type UnattendedStatus,
} from './unattended-settings';

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
// Host-side unattended access and address book (§8; ADR 0033, ADR 0034).
// Both start from the safest reading: access off, nobody trusted. A poll that
// fails leaves them there rather than claiming something is on.
let unattended: UnattendedStatus = { enabled: false, totp_enabled: false, role: 'view_only' };
let savedDevices: AddressBookEntry[] = [];
// What this machine can do about producing a picture. Both true until
// `network_status` says otherwise, so a host that is fine shows no warning.
let canCapture = true;
let canEncode = true;
// When each peer last synced a clipboard, by pseudonymized label. Only the
// timestamp is kept: `clipboard_pull` hands back the text, and this is where
// that text stops — nothing renders it and nothing stores it (§15).
const clipboardSyncedAt = new Map<string, number>();
// Where each running recording is being written, by peer label. Filled from
// what the actor answered when the recording started: the path is chosen in
// Rust and shown here, never chosen here (§2.3).
const recordingPaths = new Map<string, string>();
// What this device has recorded, from the last poll (§9.2; ADR 0035). Names
// only: the directory they live in stays in Rust, and the export takes one
// of these names back rather than a path.
let recordings: RecordingEntry[] = [];
// Offers waiting for an answer and transfers in flight, from the last poll.
let files: FileTransfers = { offers: [], transfers: [] };

/**
 * Warns the operator that people they invite will see nothing.
 *
 * The session still connects and input still works, so this is a warning and
 * not an error state — but it has to be on this screen: without it the only
 * symptom is on the *guest's* screen, a minute later, as a connection failure
 * that never happened.
 */
/**
 * Says, unconditionally, that this device is recording somebody (§17).
 *
 * Sits next to the media warning rather than inside the session list because
 * it must be visible whether or not the list is scrolled to the session doing
 * the recording. "No hidden capture" is a rule about what the person at this
 * machine can see without looking for it (§2.2).
 */
function recordingBanner(): TemplateResult | typeof nothing {
  if (!sessions.some((session) => session.recording_active)) {
    return nothing;
  }
  return html`<p class="recording-banner" role="status" aria-live="polite" data-testid="recording-banner">
    <span class="recording-dot" aria-hidden="true"></span>${t(locale, 'status.recording.banner')}
  </p>`;
}

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
              ${unattendedIndicator(unattended, locale)} ${recordingBanner()} ${mediaWarning()}
              ${isAwaitingCredentials() ? credentialsPanel(locale) : ''} ${connectPanel(locale)}
              <div class="main-divider"></div>
              ${sessionStatus(
                activeSessions,
                locale,
                () => void refresh(),
                history,
                (peer) => void reconnect(peer),
                isConnecting(),
                openChat,
                clipboardSyncedAt,
                files,
                undefined,
                recordingPaths,
                (peer, path) => {
                  if (path === null) {
                    recordingPaths.delete(peer);
                  } else {
                    recordingPaths.set(peer, path);
                  }
                },
                (peer) => saveDeviceButton(peer, locale, () => void refresh()),
              )}
              <div class="main-divider"></div>
              ${recordingsPanel(recordings, locale, tauriRecordingsCommands, () => void refresh())}
              <div class="main-divider"></div>
              ${addressBook(savedDevices, locale, () => void refresh())}
              ${unattendedSettings(unattended, locale, () => void refresh())}
            </main>
          </div>
        </div>
      `,
      consentDialog(pendingRequest, locale),
    ],
    root as HTMLElement,
  );
}

/**
 * Asks each active session whether a clipboard arrived since the last poll.
 *
 * A pull rather than a broadcast, and the returned text is dropped on the
 * floor here: it has already been applied to this machine's clipboard by the
 * Rust actor, and putting it on any surface the UI can render — or into the
 * event bus every listener sees — is exactly what §15 rules out for clipboard
 * content.
 */
async function noteClipboardArrivals(
  invoke: <T>(command: string, args?: Record<string, unknown>) => Promise<T>,
): Promise<void> {
  const active = sessions.filter((session) => session.state === 'active');
  for (const peer of active.map((session) => session.peer_label)) {
    try {
      if ((await invoke<string | null>('clipboard_pull', { peer })) !== null) {
        clipboardSyncedAt.set(peer, Date.now());
      }
    } catch {
      // The session ended between the two calls; nothing to note.
    }
  }
  for (const peer of [...clipboardSyncedAt.keys()]) {
    if (!active.some((session) => session.peer_label === peer)) {
      clipboardSyncedAt.delete(peer);
    }
  }
}

async function refresh(): Promise<void> {
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    const [sessionResult, historyResult, networkResult, connectResult, unattendedResult, bookResult] =
      await Promise.all([
        invoke<SessionStatus[]>('session_status'),
        invoke<HistoryEntry[]>('connection_history'),
        invoke<{ ready: boolean; can_capture: boolean; can_encode: boolean }>('network_status'),
        invoke<{
          phase: ConnectPhase;
          pending: boolean;
          code: string | null;
          code_required: boolean;
          retry_secs: number | null;
        }>('connect_status'),
        invoke<UnattendedStatus>('unattended_status'),
        invoke<AddressBookEntry[]>('address_book_list'),
      ]);
    // Its own call rather than part of the batch above: a transfer list is
    // only interesting once a session exists, and a failure here must not
    // cost the session list its refresh.
    try {
      files = await invoke<FileTransfers>('file_transfers');
    } catch (error) {
      console.error('file_transfers failed:', error);
    }
    // Same reasoning, and the same isolation: a directory that cannot be read
    // must not cost the session list its refresh.
    try {
      recordings = await invoke<RecordingEntry[]>('recordings_list');
    } catch (error) {
      console.error('recordings_list failed:', error);
    }
    sessions = sessionResult;
    history = historyResult;
    unattended = unattendedResult;
    savedDevices = bookResult;
    networkReady = networkResult.ready;
    canCapture = networkResult.can_capture;
    canEncode = networkResult.can_encode;
    // The peer the drawer is open on ended its session: close rather than
    // leave a transcript poll spinning on a dead label (the poll itself stops
    // on the first IPC error, but this keeps the UI honest immediately).
    if (chatPeer && !sessions.some((s) => s.state === 'active' && s.peer_label === chatPeer)) {
      closeChat();
    }
    // A recording cannot outlive the session it covers (§8.2), so neither may
    // the path shown for it.
    for (const peer of [...recordingPaths.keys()]) {
      if (!sessions.some((s) => s.peer_label === peer && s.recording_active)) {
        recordingPaths.delete(peer);
      }
    }
    // The connect form's own wait: a dial that returned is not a session yet,
    // and only the actor knows whether the far side has decided (§21 item 6).
    setConnectPhase(
      connectResult.phase,
      connectResult.code,
      connectResult.code_required,
      connectResult.retry_secs,
    );
    await noteClipboardArrivals(invoke);
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
onUnattendedStateChange(renderNow);
onAddressBookStateChange(renderNow);
onRecordingsStateChange(renderNow);

renderNow();
void refresh();
setInterval(() => {
  void refresh();
}, 1000);
