// Active session indicator (design doc §15, §21).
//
// While anyone is connected this must stay visible, and revoke must be one
// click away. Peers are shown by pseudonymized label: raw identities never
// reach the UI.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';
import type { Role } from './consent-dialog';

export interface SessionStatus {
  peer_label: string;
  role: Role;
  input: boolean;
}

async function revoke(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_revoke', { args: { peer } });
}

const roleKey: Record<Role, 'status.role.viewOnly' | 'status.role.controlLimited' | 'status.role.fullControl'> = {
  view_only: 'status.role.viewOnly',
  control_limited: 'status.role.controlLimited',
  full_control: 'status.role.fullControl',
};

export function sessionStatus(sessions: SessionStatus[], locale: Locale): TemplateResult {
  if (sessions.length === 0) {
    return html`<section class="status" aria-live="polite"><p>${t(locale, 'status.notSharing')}</p></section>`;
  }

  return html`
    <section class="status" aria-live="polite">
      <h2>${t(locale, 'status.heading')}</h2>
      <ul>
        ${sessions.map(
          (session) => html`
            <li>
              <span>${session.peer_label}</span>
              <span>${t(locale, roleKey[session.role])}</span>
              <span>${session.input ? t(locale, 'status.inputOn') : t(locale, 'status.inputOff')}</span>
              <button type="button" @click=${() => void revoke(session.peer_label)}>
                ${t(locale, 'status.revoke')}
              </button>
            </li>
          `,
        )}
      </ul>
    </section>
  `;
}
