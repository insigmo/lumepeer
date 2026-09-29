// Active session indicator (design doc §15, §21).
//
// While anyone is connected this must stay visible, and revoke must be one
// click away. Peers are shown by the machine name they gave (ADR 0121) with
// the pseudonymized label as its tooltip, or by the label alone when they gave
// none: raw identities never reach the UI.
//
// A card holds what is true about a session; what can be done to it is in the
// card's menu. The exceptions are the questions a person has to answer — a
// guest asking to be recorded, a file waiting to be accepted — and the mark
// that a guest wrote something (ADR 0120), which are not settings and must
// not be one click out of sight.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';
import type { Role } from './consent-dialog';
import type { ConnectionStats } from './connection-quality';
import { connectionQuality } from './connection-quality';
import type { FileCommands, FileTransfers } from './file-transfers';
import { forgetThumbnail, thumbnailOf } from './peer-thumbnails';
import { fileTransferPanel, tauriFileCommands } from './file-transfers';
import { peerDisplayName, peerName } from './peer-name';

export type SessionState = 'pending' | 'active';

/**
 * The permissions of §8.2 the host can move on a session that is already
 * running. `view` and `input` are absent on purpose: they follow the role, and
 * this window has no way to reach them.
 *
 * A session starts holding whatever its role brings — everything for full
 * control, nothing but `view` for a guest that is only watching — so there
 * are no per-grant switches on this panel any more (ADR 0054). The names stay
 * because the core still moves each one on its own.
 */
export type IndependentGrant =
  | 'clipboard_read'
  | 'clipboard_write'
  | 'file_transfer'
  | 'recording'
  | 'display_mode'
  | 'secure_desktop'
  | 'secure_desktop_input'
  | 'tunnel'
  | 'terminal';

export interface SessionStatus {
  peer_label: string;
  /**
   * What the guest said its machine is called, and the operating system it
   * runs (ADR 0121). Absent until it has said so, and from a guest too old
   * to: the card then shows the label, as it always did.
   */
  device_name?: string | null;
  device_os?: string | null;
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
  /**
   * Whether this guest may see the host's secure desktop (UAC prompt, lock
   * screen, fast user switch) instead of the honest "can't see this" message
   * (ADR 0049). Carried by full control, absent from every lesser role.
   */
  secure_desktop: boolean;
  /**
   * Whether this guest may inject input into the host's secure desktop — click
   * the UAC prompt, type into the lock screen (ADR 0057). Unlike every other
   * grant it is carried by no role, not even full control, so the switch below
   * is the only way it ever turns on.
   */
  secure_desktop_input: boolean;
  /**
   * Whether this guest is, right now, actually seeing it. Not the same as
   * `secure_desktop`, which is only permission — the host's non-removable
   * indicator hangs off this one, the same way the recording dot hangs off
   * `recording_active` rather than `recording`.
   */
  secure_desktop_active: boolean;
  /**
   * Whether this guest may forward TCP connections into the host's own
   * network (ADR 0078).
   *
   * Permission only, and half a decision: which addresses it may reach is a
   * separate list the host writes one entry at a time, and this flag with an
   * empty list reaches nothing.
   */
  tunnel: boolean;
  /**
   * Whether this guest may start a shell on this host (ADR 0079).
   *
   * Permission only. `input` does not imply it and it does not imply `input`:
   * a host that handed over the keyboard can watch what happens on its own
   * screen, and a shell is not on the screen.
   */
  terminal: boolean;
  /**
   * Whether this guest has a shell running right now. Not the same as
   * `terminal`, which is only permission — the host's non-removable indicator
   * hangs off this one, the same way the recording dot hangs off
   * `recording_active`.
   */
  terminal_active: boolean;
  /**
   * Whether this guest wrote in the chat and nobody here has opened it yet.
   * Kept by the actor rather than by this window, so the session bar and the
   * main window agree on it and opening the drawer clears it for both.
   */
  chat_unread: boolean;
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
  /**
   * What the host said its machine is called the last time it admitted this
   * device, and its operating system (ADR 0121); absent for a host too old to
   * say.
   */
  device_name?: string | null;
  device_os?: string | null;
  role: Role;
  /**
   * Unix seconds this row was last written — a connect or a disconnect,
   * whichever happened most recently (docs/bugs/03-connection-list.md,
   * task 4).
   */
  last_seen_at: number;
  /**
   * Whether this device has a password saved for that host (§8; ADR 0033),
   * so the row can offer to forget it. Never the password itself.
   */
  has_password?: boolean;
  /**
   * Whether this node may dial that host again by itself after the link goes
   * away (ADR 0084).
   *
   * Its own decision, and nothing else sets it: connecting to a host, being
   * granted a role and saving its password all leave it alone. Optional so a
   * row from an older answer reads as "no", which is the safe direction.
   */
  trusted?: boolean;
}

async function revoke(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_revoke', { args: { peer } });
}

/**
 * Switches unprompted reconnection to a remembered host on or off (ADR 0084).
 *
 * Widens nothing on the far side: what comes back is a new session that host
 * decides from scratch. What it permits is the asking, from here, without a
 * person pressing the button.
 */
async function setAutoReconnect(peer: string, trusted: boolean): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('history_set_trusted', { args: { peer, trusted } });
}

/** Forgets a remembered host (docs/bugs/03-connection-list.md, task 5). */
async function forgetHistory(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('history_remove', { args: { peer } });
}

/**
 * Drops the saved device password for one host without deleting its row.
 *
 * Removing the row does this as well; this is for the case where the host is
 * still worth keeping in the list but signing in unasked is not.
 */
async function forgetPassword(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('history_forget_password', { args: { peer } });
}

/**
 * Moves one independent grant on a running session (§8.2).
 *
 * The host no longer has a switch per grant: a session starts with exactly
 * what its role brings (`Grants::from_role`), so the only place left that
 * changes one is the record request below, where saying "allow" to a guest
 * that asked to be recorded is the host granting `recording` in the same
 * press. Nothing is toggled locally either way — the core decides, and
 * `onChange` re-polls for what it decided.
 */
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
 * Runs one recording change and re-polls, whatever the core answered.
 *
 * Refused, or the session ended between poll and press: the re-poll is what
 * keeps a control from claiming something the core did not do.
 */
function recordingChange(
  peer: string,
  onChange: () => void,
  onPath: (peer: string, path: string | null) => void,
): (promise: Promise<string | null>) => void {
  return (promise) => {
    void promise.then(
      (next) => {
        onPath(peer, next);
        onChange();
      },
      (error: unknown) => {
        console.error('recording_toggle failed:', error);
        onChange();
      },
    );
  };
}

/**
 * Starting and stopping the recording of one active session (§17), as an item
 * of the card's menu.
 *
 * Unreachable without the `recording` grant: the host turns the permission on
 * first and records second, and nothing here can skip that order. Whether a
 * recording is running is not said here but on the card, where the indicator
 * both sides must see cannot be folded away.
 */
function recordingMenuItem(
  session: SessionStatus,
  locale: Locale,
  onChange: () => void,
  onPath: (peer: string, path: string | null) => void,
): TemplateResult {
  const peer = session.peer_label;
  const running = session.recording_active;
  const change = recordingChange(peer, onChange, onPath);
  return html`
    <button
      type="button"
      class="peer-menu-item peer-menu-record ${running ? 'is-recording' : ''}"
      role="menuitem"
      data-testid="record-toggle"
      ?disabled=${!session.recording}
      title=${session.recording ? '' : t(locale, 'status.recording.needsGrant')}
      @click=${(event: Event) => {
        closeMenu(event);
        change(toggleRecording(peer, !running));
      }}
    >
      ${t(locale, running ? 'status.recording.stop' : 'status.recording.start')}
    </button>
  `;
}

/** The recording indicator (§17): on while the core says a recording runs. */
function recordingIndicator(
  session: SessionStatus,
  locale: Locale,
  path: string | undefined,
): TemplateResult | '' {
  if (!session.recording_active) {
    return '';
  }
  return html`
    <span class="recording-indicator" role="status" data-testid="recording-indicator">
      <span class="recording-dot" aria-hidden="true"></span>${t(locale, 'status.recording.on')}
    </span>
    ${path
      ? html`<span class="recording-path" data-testid="recording-path" title=${path}
          >${path.split(/[\\/]/).pop() ?? path}</span
        >`
      : ''}
  `;
}

/**
 * A guest waiting to be recorded (§17): a question for the person at this
 * machine, so it sits on the card rather than in the menu. Saying yes grants
 * `recording` and starts it in the same press; never auto-answered.
 */
function recordRequest(
  session: SessionStatus,
  locale: Locale,
  onChange: () => void,
  onPath: (peer: string, path: string | null) => void,
): TemplateResult | '' {
  const peer = session.peer_label;
  const change = recordingChange(peer, onChange, onPath);
  return session.record_request
    ? html`
        <div class="record-request" role="status" data-testid="record-request">
          <span>${t(locale, 'status.recording.requested', peerDisplayName(session))}</span>
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
    : '';
}

/**
 * The secure-desktop indicator (ADR 0049, §17-equivalent).
 *
 * No settings switch hides this: while a guest is actually seeing the
 * secure desktop, the host sees that it is happening, exactly as the
 * recording indicator above cannot be turned off while a recording runs.
 * There is no start/stop button here — unlike recording, showing the
 * secure desktop is not something the host presses a button for; it
 * happens on its own, on a session that already holds the grant, whenever
 * capture on that host gets stuck behind one.
 */
function secureDesktopIndicator(session: SessionStatus, locale: Locale): TemplateResult | '' {
  return session.secure_desktop_active
    ? html`<span class="secure-desktop-indicator" role="status" data-testid="secure-desktop-indicator">
        <span class="secure-desktop-dot" aria-hidden="true"></span>${t(locale, 'status.secureDesktop.active')}
      </span>`
    : '';
}

/**
 * The terminal indicator (ADR 0079 decision 3).
 *
 * A terminal is the one capability with no picture attached to it: an
 * operator watching the screen can see a guest type, and cannot see a shell.
 * So while one is running the host is told, on the session's own row and on
 * the always-on-top bar, and no setting switches either off — exactly as the
 * recording dot and the secure-desktop dot cannot be switched off while what
 * they are about is happening. It hangs off `terminal_active` rather than the
 * grant, because permission is not what is worth interrupting somebody for.
 */
function terminalIndicator(session: SessionStatus, locale: Locale): TemplateResult | '' {
  return session.terminal_active
    ? html`<span class="terminal-indicator" role="status" data-testid="terminal-indicator">
        <span class="terminal-dot" aria-hidden="true"></span>${t(locale, 'status.terminal.active')}
      </span>`
    : '';
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

/**
 * The picture of a remembered host, or the glyph that stands in until a
 * session has been watched long enough to take one (ADR 0094;
 * `peer-thumbnails.ts`).
 *
 * A screen rather than an operating-system logo on purpose: which machine
 * this is, is a thing people recognize by what was on it, and the logo of an
 * OS is the one fact every row would have in common. The OS goes beside the
 * name instead (ADR 0121).
 */
function peerThumbnail(host: string, locale: Locale): TemplateResult {
  const image = thumbnailOf(host);
  return html`
    <span class="peer-thumb" data-testid="peer-thumb">
      ${image === null
        ? html`
            <svg width="34" height="34" viewBox="0 0 24 24" aria-hidden="true" class="peer-thumb-glyph">
              <rect x="2.5" y="4" width="19" height="13" rx="1.6" stroke="currentColor" stroke-width="1.4" fill="none" />
              <line x1="8" y1="20" x2="16" y2="20" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" />
              <line x1="12" y1="17" x2="12" y2="20" stroke="currentColor" stroke-width="1.4" />
            </svg>
          `
        : html`<img class="peer-thumb-image" src=${image} alt=${t(locale, 'connections.preview')} />`}
    </span>
  `;
}

/** Closes the menu the pressed item belongs to, whatever the item then does. */
function closeMenu(event: Event): void {
  (event.currentTarget as HTMLElement | null)?.closest('details.peer-menu')?.removeAttribute('open');
}

/**
 * The card's overflow menu: the three dots, and everything this row can do.
 *
 * A `<details>` rather than a hand-rolled popup because this panel re-renders
 * every second — anything whose openness lived in a template variable would
 * shut itself while being read. The browser owns the `open` attribute, and
 * lit-html leaves attributes it does not bind alone.
 */
function peerMenu(label: string, locale: Locale, items: (TemplateResult | '')[]): TemplateResult {
  return html`
    <details class="peer-menu" data-testid="peer-menu">
      <summary
        class="peer-menu-btn"
        aria-haspopup="menu"
        aria-label=${`${t(locale, 'connections.actions')}: ${label}`}
        title=${t(locale, 'connections.actions')}
      >
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
          <circle cx="8" cy="3.2" r="1.5" fill="currentColor" />
          <circle cx="8" cy="8" r="1.5" fill="currentColor" />
          <circle cx="8" cy="12.8" r="1.5" fill="currentColor" />
        </svg>
      </summary>
      <div class="peer-menu-list" role="menu">${items}</div>
    </details>
  `;
}

/**
 * What a dial to a remembered host opens: its screen, a shell and nothing
 * else (ADR 0101), or its files and nothing else (ADR 0124). The last two
 * never open the media connection, so the host encodes nothing.
 */
export type ConnectSurface = 'screen' | 'terminal' | 'files';

export function sessionStatus(
  sessions: SessionStatus[],
  locale: Locale,
  onRefresh: () => void = () => {},
  history: HistoryEntry[] = [],
  /** Dials a remembered host again, for the surface named. */
  onReconnect: (peer: string, surface?: ConnectSurface) => void = () => {},
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
   * Saving trusts the device (ADR 0120): the host pressing it is the host
   * naming the device in advance. Withdrawing that is one untick on the
   * address-book panel, and having merely connected is still never a path to
   * a permission (§2.1).
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
          <ul class="connections-list connections-grid" aria-live="polite">
            ${sessions.map((session) => {
              const peer = session.peer_label;
              const name = peerDisplayName(session);
              const active = session.state === 'active';
              const clipboardSynced = Date.now() - (clipboardSyncedAt.get(peer) ?? 0) < CLIPBOARD_NOTE_MS;
              // What is true about the session right now, in one wrapping row
              // under its name; the row is left out when there is nothing to
              // say, so a quiet session is a quiet card.
              const hasStatus =
                clipboardSynced ||
                (active &&
                  (connectionStats.has(peer) ||
                    session.recording_active ||
                    session.secure_desktop_active ||
                    session.terminal_active));
              // Files appear once there is something to accept or to watch;
              // sending needs no control of its own (copying is sending).
              const hasFiles =
                files.offers.some((offer) => offer.peer_label === peer) ||
                files.transfers.some((row) => row.peer_label === peer);
              return html`
                <li class="peer-card" data-testid="session-card">
                  <div class="peer-card-face">
                    ${peerThumbnail('', locale)}
                    <span class="peer-card-caption">
                      <span class="peer-meta">${t(locale, roleKey[session.role])}</span>
                      <span class="peer-meta"
                        >${session.input ? t(locale, 'status.inputOn') : t(locale, 'status.inputOff')}</span
                      >
                    </span>
                  </div>
                  <div class="peer-card-foot">
                    <span
                      class="peer-dot"
                      data-state=${active ? 'live' : 'pending'}
                      title=${t(locale, active ? 'connections.live' : 'connections.idle')}
                    ></span>
                    ${peerName(session, locale)}
                    ${active && session.chat_unread
                      ? html`<button
                          type="button"
                          class="chat-unread-btn"
                          data-testid="chat-unread"
                          aria-label=${`${t(locale, 'toolbar.chat.unread')}: ${name}`}
                          title=${t(locale, 'toolbar.chat.unread')}
                          @click=${() => onOpenChat(peer)}
                        >
                          <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
                            <path
                              d="M3 3h10a1 1 0 0 1 1 1v6a1 1 0 0 1-1 1H7l-3 3v-3H3a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1Z"
                              fill="none"
                              stroke="currentColor"
                              stroke-width="1.5"
                              stroke-linejoin="round"
                            />
                            <circle class="chat-unread-dot" cx="13" cy="3" r="3" />
                          </svg>
                        </button>`
                      : ''}
                    ${peerMenu(name, locale, [
                      active
                        ? html`
                            <button
                              type="button"
                              class="peer-menu-item chat-open-btn"
                              role="menuitem"
                              aria-label=${`${t(locale, 'chat.open')}: ${name}`}
                              @click=${(event: Event) => {
                                closeMenu(event);
                                onOpenChat(peer);
                              }}
                            >
                              ${t(locale, 'chat.open')}
                            </button>
                          `
                        : '',
                      active ? recordingMenuItem(session, locale, onRefresh, onRecordingPath) : '',
                      active ? html`<span class="peer-menu-item" role="menuitem">${saveDevice(peer)}</span>` : '',
                      html`
                        <button
                          type="button"
                          class="peer-menu-item is-destructive revoke-btn"
                          role="menuitem"
                          @click=${(event: Event) => {
                            closeMenu(event);
                            void revoke(peer);
                          }}
                        >
                          ${t(locale, 'status.revoke')}
                        </button>
                      `,
                    ])}
                  </div>
                  ${hasStatus
                    ? html`
                        <div class="peer-card-status" data-testid="session-status-row">
                          ${active ? connectionQuality(connectionStats.get(peer), locale) : ''}
                          ${active ? recordingIndicator(session, locale, recordingPaths.get(peer)) : ''}
                          ${active ? secureDesktopIndicator(session, locale) : ''}
                          ${active ? terminalIndicator(session, locale) : ''}
                          ${clipboardSynced
                            ? html`<span class="clipboard-note" role="status" data-testid="clipboard-note"
                                >${t(locale, 'status.clipboardSynced')}</span
                              >`
                            : ''}
                        </div>
                      `
                    : ''}
                  ${active ? recordRequest(session, locale, onRefresh, onRecordingPath) : ''}
                  ${active && session.file_transfer && hasFiles
                    ? fileTransferPanel(peer, files, locale, fileCommands, onRefresh)
                    : ''}
                </li>
              `;
            })}
            ${history.map((entry) => {
              const name = peerDisplayName(entry);
              return html`
                <li class="peer-card history-row" data-testid="history-card">
                  <button
                    type="button"
                    class="peer-card-face history-reconnect"
                    ?disabled=${reconnectDisabled}
                    title=${t(locale, 'status.reconnect')}
                    aria-label=${`${t(locale, 'status.reconnect')}: ${name}`}
                    @click=${() => onReconnect(entry.peer_label)}
                  >
                    ${peerThumbnail(entry.peer_label, locale)}
                    <span class="peer-card-caption">
                      <span class="peer-meta">${t(locale, roleKey[entry.role])}</span>
                      <span class="peer-meta history-ended">${relativeTime(entry.last_seen_at, locale)}</span>
                    </span>
                  </button>
                  <div class="peer-card-foot">
                    <span
                      class="peer-dot"
                      data-state="idle"
                      title=${t(locale, 'connections.idle')}
                    ></span>
                    ${peerName(entry, locale)}
                    ${peerMenu(name, locale, [
                      html`
                        <button
                          type="button"
                          class="peer-menu-item peer-menu-connect"
                          role="menuitem"
                          ?disabled=${reconnectDisabled}
                          @click=${(event: Event) => {
                            closeMenu(event);
                            onReconnect(entry.peer_label);
                          }}
                        >
                          ${t(locale, 'status.reconnect')}
                        </button>
                      `,
                      html`
                        <button
                          type="button"
                          class="peer-menu-item peer-menu-connect-terminal"
                          role="menuitem"
                          data-testid="history-connect-terminal"
                          ?disabled=${reconnectDisabled || entry.role !== 'full_control'}
                          title=${entry.role === 'full_control'
                            ? ''
                            : t(locale, 'status.reconnectTerminal.needsFullControl')}
                          @click=${(event: Event) => {
                            closeMenu(event);
                            onReconnect(entry.peer_label, 'terminal');
                          }}
                        >
                          ${t(locale, 'status.reconnectTerminal')}
                        </button>
                      `,
                      // Not gated on the role, unlike the terminal: file
                      // browsing is a grant of its own the host can give any
                      // role, and the file manager says so itself when it
                      // was not given (ADR 0124).
                      html`
                        <button
                          type="button"
                          class="peer-menu-item peer-menu-open-files"
                          role="menuitem"
                          data-testid="history-open-files"
                          ?disabled=${reconnectDisabled}
                          @click=${(event: Event) => {
                            closeMenu(event);
                            onReconnect(entry.peer_label, 'files');
                          }}
                        >
                          ${t(locale, 'fileManager.heading')}
                        </button>
                      `,
                      html`
                        <label
                          class="peer-menu-item history-autoreconnect"
                          role="menuitem"
                          title=${t(locale, 'history.autoReconnect.hint')}
                          data-testid="history-auto-reconnect"
                        >
                          <input
                            type="checkbox"
                            .checked=${entry.trusted === true}
                            aria-label=${`${t(locale, 'history.autoReconnect')}: ${name}`}
                            @change=${(event: Event) => {
                              const on = (event.target as HTMLInputElement).checked;
                              void setAutoReconnect(entry.peer_label, on).then(onRefresh, (error: unknown) => {
                                console.error('history_set_trusted failed:', error);
                                onRefresh();
                              });
                            }}
                          />
                          <span>${t(locale, 'history.autoReconnect')}</span>
                        </label>
                      `,
                      entry.has_password
                        ? html`<button
                            type="button"
                            class="peer-menu-item history-forget-password"
                            role="menuitem"
                            title=${t(locale, 'history.forgetPassword.hint')}
                            aria-label=${`${t(locale, 'history.forgetPassword')}: ${name}`}
                            @click=${(event: Event) => {
                              closeMenu(event);
                              if (!globalThis.confirm(t(locale, 'history.forgetPassword.confirm', name))) {
                                return;
                              }
                              void forgetPassword(entry.peer_label).then(onRefresh, (error: unknown) => {
                                console.error('history_forget_password failed:', error);
                                onRefresh();
                              });
                            }}
                          >
                            ${t(locale, 'history.forgetPassword')}
                          </button>`
                        : '',
                      html`
                        <button
                          type="button"
                          class="peer-menu-item is-destructive history-remove"
                          role="menuitem"
                          aria-label=${`${t(locale, 'history.remove')}: ${name}`}
                          @click=${(event: Event) => {
                            closeMenu(event);
                            if (!globalThis.confirm(t(locale, 'history.remove.confirm', name))) {
                              return;
                            }
                            forgetThumbnail(entry.peer_label);
                            void forgetHistory(entry.peer_label).then(onRefresh, (error: unknown) => {
                              console.error('history_remove failed:', error);
                              onRefresh();
                            });
                          }}
                        >
                          ${t(locale, 'history.remove')}
                        </button>
                      `,
                    ])}
                  </div>
                </li>
              `;
            })}
          </ul>
        `}
  `;
}
