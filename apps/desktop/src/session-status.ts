// Active session indicator (design doc §15, §21).
//
// While anyone is connected this must stay visible, and revoke must be one
// click away. Peers are shown by pseudonymized label: raw identities never
// reach the UI.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';
import type { Role } from './consent-dialog';
import type { ConnectionStats } from './connection-quality';
import { connectionQuality } from './connection-quality';
import type { FileCommands, FileTransfers } from './file-transfers';
import { fileTransferPanel, tauriFileCommands } from './file-transfers';

export type SessionState = 'pending' | 'active';

/**
 * The permissions of §8.2 the host can move on a session that is already
 * running. `view` and `input` are absent on purpose: they follow the role, and
 * this window has no way to reach them. `display_mode` is the newest one
 * (docs/bugs/16-host-display-mode.md; ADR 0048): switching this host's own
 * monitor is materially riskier than anything else on this list, so it stays
 * its own independent grant rather than riding along with any role.
 */
export type IndependentGrant =
  | 'clipboard_read'
  | 'clipboard_write'
  | 'file_transfer'
  | 'recording'
  | 'display_mode';

export interface SessionStatus {
  peer_label: string;
  role: Role;
  input: boolean;
  state: SessionState;
  clipboard_read: boolean;
  clipboard_write: boolean;
  file_transfer: boolean;
  recording: boolean;
  display_mode: boolean;
  /**
   * Whether a recording is being written right now (§17).
   *
   * Not the same as `recording`, which is only permission. The indicator both
   * sides must show hangs off this one.
   */
  recording_active: boolean;
  /** Whether this guest asked to be recorded and is still waiting (§17). */
  record_request: boolean;
}

/**
 * How long the "clipboard synced" note stays up after a payload arrives.
 *
 * Long enough to be noticed between two one-second polls, short enough that
 * the row does not keep claiming something that happened a minute ago.
 */
export const CLIPBOARD_NOTE_MS = 4000;

/**
 * One row of `connection_history` (§21 punch-list item 5): a host this device
 * has connected to before, and can go back to.
 *
 * The invite code behind the row stays in Rust — clicking it names the host by
 * label and the actor looks the code up (§13).
 */
export interface HistoryEntry {
  peer_label: string;
  role: Role;
  /**
   * Unix seconds this row was last written — a connect or a disconnect,
   * whichever happened most recently (docs/bugs/03-connection-list.md,
   * task 4).
   */
  last_seen_at: number;
}

async function revoke(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_revoke', { args: { peer } });
}

/** Forgets a remembered host (docs/bugs/03-connection-list.md, task 5). */
async function forgetHistory(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('history_remove', { args: { peer } });
}

async function setGrant(peer: string, grant: IndependentGrant, allowed: boolean): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_set_grant', { args: { peer, grant, allowed } });
}

/**
 * Starts or stops the recording of `peer` (§17).
 *
 * Answers with the file the *actor* chose. The webview never names a path:
 * where this machine writes is not a decision the untrusted view layer takes
 * (§2.3), so the path only ever travels outwards, to be shown.
 */
async function toggleRecording(peer: string, on: boolean): Promise<string | null> {
  const { invoke } = await import('@tauri-apps/api/core');
  return (await invoke('recording_toggle', { args: { peer, on } })) as string | null;
}

/**
 * The switches, in the order they are shown. The label says what the guest
 * gets, so that turning one on is a decision about a consequence rather than
 * about a flag name (§2.2, §19 phase 6).
 */
const GRANT_ROWS: readonly {
  grant: IndependentGrant;
  key:
    | 'status.grants.clipboardRead'
    | 'status.grants.clipboardWrite'
    | 'status.grants.fileTransfer'
    | 'status.grants.recording'
    | 'status.grants.displayMode';
  held: (session: SessionStatus) => boolean;
}[] = [
  { grant: 'clipboard_read', key: 'status.grants.clipboardRead', held: (s) => s.clipboard_read },
  { grant: 'clipboard_write', key: 'status.grants.clipboardWrite', held: (s) => s.clipboard_write },
  { grant: 'file_transfer', key: 'status.grants.fileTransfer', held: (s) => s.file_transfer },
  { grant: 'recording', key: 'status.grants.recording', held: (s) => s.recording },
  { grant: 'display_mode', key: 'status.grants.displayMode', held: (s) => s.display_mode },
];

/**
 * The independent grants of one active session.
 *
 * Nothing is toggled locally: the checkbox shows what the last `session_status`
 * said the core holds, the click asks the core to change it, and `onChange`
 * re-polls. A switch that looks on while the host refused would be the one
 * lie this panel must never tell.
 */
function grantSwitches(
  session: SessionStatus,
  locale: Locale,
  onChange: () => void,
): TemplateResult {
  return html`
    <fieldset class="session-grants">
      <legend>${t(locale, 'status.grants.heading')}</legend>
      ${GRANT_ROWS.map(
        (row) => html`
          <label class="grant-row">
            <input
              type="checkbox"
              .checked=${row.held(session)}
              aria-label=${`${t(locale, row.key)}: ${session.peer_label}`}
              @change=${(event: Event) => {
                const allowed = (event.target as HTMLInputElement).checked;
                void setGrant(session.peer_label, row.grant, allowed).then(onChange, (error: unknown) => {
                  console.error('session_set_grant failed:', error);
                  onChange();
                });
              }}
            />
            <span>${t(locale, row.key)}</span>
          </label>
        `,
      )}
    </fieldset>
  `;
}

/**
 * The recording row of one active session (§17).
 *
 * Three things in one place, because they are one decision: whether recording
 * is permitted at all (the grant switch above), whether it is running, and —
 * while a guest is waiting — whether to say yes. The button is unreachable
 * without the `recording` grant: the host turns the permission on first and
 * records second, and nothing here can skip that order.
 */
function recordingRow(
  session: SessionStatus,
  locale: Locale,
  onChange: () => void,
  path: string | undefined,
  onPath: (peer: string, path: string | null) => void,
): TemplateResult {
  const peer = session.peer_label;
  const running = session.recording_active;
  const change = (promise: Promise<string | null>): void => {
    void promise.then(
      (next) => {
        onPath(peer, next);
        onChange();
      },
      (error: unknown) => {
        // Refused, or the session ended between poll and press: re-poll rather
        // than leave a button claiming something the core did not do.
        console.error('recording_toggle failed:', error);
        onChange();
      },
    );
  };
  return html`
    <div class="session-recording">
      <button
        type="button"
        class="record-btn ${running ? 'is-recording' : ''}"
        data-testid="record-toggle"
        ?disabled=${!session.recording}
        aria-pressed=${running ? 'true' : 'false'}
        title=${session.recording ? '' : t(locale, 'status.recording.needsGrant')}
        @click=${() => change(toggleRecording(peer, !running))}
      >
        ${t(locale, running ? 'status.recording.stop' : 'status.recording.start')}
      </button>
      ${running
        ? html`<span class="recording-indicator" role="status" data-testid="recording-indicator">
            <span class="recording-dot" aria-hidden="true"></span>${t(locale, 'status.recording.on')}
          </span>`
        : ''}
      ${running && path
        ? html`<span class="recording-path" data-testid="recording-path" title=${path}
            >${path.split(/[\\/]/).pop() ?? path}</span
          >`
        : ''}
    </div>
    ${session.record_request
      ? html`
          <div class="record-request" role="status" data-testid="record-request">
            <span>${t(locale, 'status.recording.requested', peer)}</span>
            <button
              type="button"
              class="record-allow"
              data-testid="record-allow"
              @click=${() =>
                change(
                  (session.recording
                    ? Promise.resolve()
                    : setGrant(peer, 'recording', true)
                  ).then(() => toggleRecording(peer, true)),
                )}
            >
              ${t(locale, 'status.recording.allow')}
            </button>
            <button
              type="button"
              class="record-deny"
              data-testid="record-deny"
              @click=${() => change(toggleRecording(peer, false))}
            >
              ${t(locale, 'status.recording.decline')}
            </button>
          </div>
        `
      : ''}
  `;
}

const MINUTE_SECS = 60;
const HOUR_SECS = 60 * MINUTE_SECS;
const DAY_SECS = 24 * HOUR_SECS;

/**
 * Coarse "how long ago" for a history row; exact enough for a sidebar list.
 *
 * Worded as "last seen", not "ended": a row is written at connect time now,
 * not only at disconnect (docs/bugs/03-connection-list.md, task 4), so a
 * value from a session still in progress must not claim the session ended.
 */
function relativeTime(lastSeenAtUnix: number, locale: Locale): string {
  const elapsed = Math.max(0, Date.now() / 1000 - lastSeenAtUnix);
  if (elapsed < MINUTE_SECS) {
    return t(locale, 'status.lastSeenJustNow');
  }
  if (elapsed < HOUR_SECS) {
    return t(locale, 'status.lastSeenMinutesAgo', String(Math.floor(elapsed / MINUTE_SECS)));
  }
  if (elapsed < DAY_SECS) {
    return t(locale, 'status.lastSeenHoursAgo', String(Math.floor(elapsed / HOUR_SECS)));
  }
  return t(locale, 'status.lastSeenDaysAgo', String(Math.floor(elapsed / DAY_SECS)));
}

const roleKey: Record<Role, 'status.role.viewOnly' | 'status.role.controlLimited' | 'status.role.fullControl'> = {
  view_only: 'status.role.viewOnly',
  control_limited: 'status.role.controlLimited',
  full_control: 'status.role.fullControl',
};

export function sessionStatus(
  sessions: SessionStatus[],
  locale: Locale,
  onRefresh: () => void = () => {},
  history: HistoryEntry[] = [],
  onReconnect: (peer: string) => void = () => {},
  reconnectDisabled = false,
  onOpenChat: (peer: string) => void = () => {},
  /**
   * When each peer last synced a clipboard, in `Date.now()` milliseconds.
   *
   * The *fact* only. §15 keeps clipboard content out of the audit log and out
   * of telemetry, and a panel that is always on screen is neither of those but
   * is read by whoever walks past the machine — so the content stays where it
   * belongs, on the clipboard.
   */
  clipboardSyncedAt: ReadonlyMap<string, number> = new Map(),
  /** Offers and transfers as the last `file_transfers` poll reported them. */
  files: FileTransfers = { offers: [], transfers: [] },
  /** How the file panel reaches the actor; injectable for tests. */
  fileCommands: FileCommands = tauriFileCommands,
  /**
   * Where each running recording is being written, by peer label.
   *
   * Filled from what the actor answered when the recording started — the path
   * is chosen in Rust and only shown here, never the other way round (§2.3).
   */
  recordingPaths: ReadonlyMap<string, string> = new Map(),
  /** Told the path of a recording that just started, or `null` when one stopped. */
  onRecordingPath: (peer: string, path: string | null) => void = () => {},
  /**
   * Renders the "save this device" control for an active session, so a host
   * can put a guest it recognises into the address book (§8; ADR 0034).
   *
   * Saving only saves. The entry lands untrusted, and trusting it is a
   * separate, confirmed decision on the address-book panel — having connected
   * once must never be a path to a permission (§2.1).
   */
  saveDevice: (peer: string) => TemplateResult | '' = () => '',
  /**
   * What each live connection's link actually looks like, by peer label
   * (§18; ADR 0026).
   *
   * Measured, never configured, and absent rather than zeroed while nothing
   * has measured it: a session with no row here simply shows no pill.
   */
  connectionStats: ReadonlyMap<string, ConnectionStats> = new Map(),
): TemplateResult {
  const empty = sessions.length === 0 && history.length === 0;
  return html`
    <div class="connections-header">
      <h2>${t(locale, 'connections.header')}</h2>
      <button type="button" class="refresh-btn" aria-label=${t(locale, 'connections.refresh')} @click=${onRefresh}>
        <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
          <path
            d="M13.5 8a5.5 5.5 0 1 1-1.6-3.89"
            stroke="currentColor"
            stroke-width="1.4"
            fill="none"
            stroke-linecap="round"
          />
          <path
            d="M13.5 2.5v3.5h-3.5"
            stroke="currentColor"
            stroke-width="1.4"
            fill="none"
            stroke-linecap="round"
            stroke-linejoin="round"
          />
        </svg>
      </button>
    </div>
    ${empty
      ? html`
          <div class="empty-state" aria-live="polite">
            <div class="empty-icon-circle" aria-hidden="true">
              <svg width="26" height="26" viewBox="0 0 24 24">
                <rect x="3" y="4" width="18" height="12" rx="1.5" stroke="currentColor" stroke-width="1.5" fill="none" />
                <line x1="8" y1="20" x2="16" y2="20" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" />
                <line x1="12" y1="16" x2="12" y2="20" stroke="currentColor" stroke-width="1.5" />
              </svg>
            </div>
            <p class="empty-title">${t(locale, 'connections.emptyTitle')}</p>
            <p class="empty-subtext">${t(locale, 'connections.emptySubtext')}</p>
          </div>
        `
      : html`
          <ul class="connections-list" aria-live="polite">
            ${sessions.map(
              (session) => html`
                <li>
                  <span class="peer-label">${session.peer_label}</span>
                  <span class="peer-meta">${t(locale, roleKey[session.role])}</span>
                  <span class="peer-meta">${session.input ? t(locale, 'status.inputOn') : t(locale, 'status.inputOff')}</span>
                  ${Date.now() - (clipboardSyncedAt.get(session.peer_label) ?? 0) < CLIPBOARD_NOTE_MS
                    ? html`<span class="clipboard-note" role="status" data-testid="clipboard-note"
                        >${t(locale, 'status.clipboardSynced')}</span
                      >`
                    : ''}
                  ${session.state === 'active'
                    ? html`
                        <button
                          type="button"
                          class="chat-open-btn"
                          aria-label=${`${t(locale, 'chat.open')}: ${session.peer_label}`}
                          @click=${() => onOpenChat(session.peer_label)}
                        >
                          ${t(locale, 'chat.open')}
                        </button>
                      `
                    : ''}
                  ${session.state === 'active' ? saveDevice(session.peer_label) : ''}
                  <button type="button" class="revoke-btn" @click=${() => void revoke(session.peer_label)}>
                    ${t(locale, 'status.revoke')}
                  </button>
                  ${session.state === 'active'
                    ? connectionQuality(connectionStats.get(session.peer_label), locale)
                    : ''}
                  ${session.state === 'active' ? grantSwitches(session, locale, onRefresh) : ''}
                  ${session.state === 'active'
                    ? recordingRow(
                        session,
                        locale,
                        onRefresh,
                        recordingPaths.get(session.peer_label),
                        onRecordingPath,
                      )
                    : ''}
                  ${session.state === 'active' && session.file_transfer
                    ? fileTransferPanel(session.peer_label, files, locale, fileCommands, onRefresh)
                    : ''}
                </li>
              `,
            )}
            ${history.map(
              (entry) => html`
                <li class="history-row">
                  <button
                    type="button"
                    class="history-reconnect"
                    ?disabled=${reconnectDisabled}
                    title=${t(locale, 'status.reconnect')}
                    @click=${() => onReconnect(entry.peer_label)}
                  >
                    <span class="peer-label">${entry.peer_label}</span>
                    <span class="peer-meta">${t(locale, roleKey[entry.role])}</span>
                    <span class="peer-meta history-ended">${relativeTime(entry.last_seen_at, locale)}</span>
                    <span class="history-action">${t(locale, 'status.reconnect')}</span>
                  </button>
                  <button
                    type="button"
                    class="history-remove"
                    aria-label=${`${t(locale, 'history.remove')}: ${entry.peer_label}`}
                    @click=${() => {
                      if (!globalThis.confirm(t(locale, 'history.remove.confirm', entry.peer_label))) {
                        return;
                      }
                      void forgetHistory(entry.peer_label).then(onRefresh, (error: unknown) => {
                        console.error('history_remove failed:', error);
                        onRefresh();
                      });
                    }}
                  >
                    ${t(locale, 'history.remove')}
                  </button>
                </li>
              `,
            )}
          </ul>
        `}
  `;
}
