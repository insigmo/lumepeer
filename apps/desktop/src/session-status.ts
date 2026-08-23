// Active session indicator (design doc §15, §21).
//
// While anyone is connected this must stay visible, and revoke must be one
// click away. Peers are shown by pseudonymized label: raw identities never
// reach the UI.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';
import type { Role } from './consent-dialog';

export type SessionState = 'pending' | 'active';

export interface SessionStatus {
  peer_label: string;
  role: Role;
  input: boolean;
  state: SessionState;
}

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
  /** Unix seconds the last session with this host ended. */
  ended_at: number;
}

async function revoke(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_revoke', { args: { peer } });
}

const MINUTE_SECS = 60;
const HOUR_SECS = 60 * MINUTE_SECS;
const DAY_SECS = 24 * HOUR_SECS;

/** Coarse "how long ago" for a history row; exact enough for a sidebar list. */
function relativeTime(endedAtUnix: number, locale: Locale): string {
  const elapsed = Math.max(0, Date.now() / 1000 - endedAtUnix);
  if (elapsed < MINUTE_SECS) {
    return t(locale, 'status.endedJustNow');
  }
  if (elapsed < HOUR_SECS) {
    return t(locale, 'status.endedMinutesAgo', String(Math.floor(elapsed / MINUTE_SECS)));
  }
  if (elapsed < DAY_SECS) {
    return t(locale, 'status.endedHoursAgo', String(Math.floor(elapsed / HOUR_SECS)));
  }
  return t(locale, 'status.endedDaysAgo', String(Math.floor(elapsed / DAY_SECS)));
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
                  <button type="button" class="revoke-btn" @click=${() => void revoke(session.peer_label)}>
                    ${t(locale, 'status.revoke')}
                  </button>
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
                    <span class="peer-meta history-ended">${relativeTime(entry.ended_at, locale)}</span>
                    <span class="history-action">${t(locale, 'status.reconnect')}</span>
                  </button>
                </li>
              `,
            )}
          </ul>
        `}
  `;
}
