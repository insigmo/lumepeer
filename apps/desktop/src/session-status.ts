// Active session indicator (design doc §15, §21).
//
// While anyone is connected this must stay visible, and revoke must be one
// click away. Peers are shown by pseudonymized label: raw identities never
// reach the UI.

import { html, type TemplateResult } from 'lit-html';

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

export function sessionStatus(sessions: SessionStatus[]): TemplateResult {
  if (sessions.length === 0) {
    return html`<section class="status" aria-live="polite"><p>Not sharing.</p></section>`;
  }

  return html`
    <section class="status" aria-live="polite">
      <h2>Active sessions</h2>
      <ul>
        ${sessions.map(
          (session) => html`
            <li>
              <span>${session.peer_label}</span>
              <span>${session.role}</span>
              <span>${session.input ? 'input on' : 'input off'}</span>
              <button type="button" @click=${() => void revoke(session.peer_label)}>
                Revoke
              </button>
            </li>
          `,
        )}
      </ul>
    </section>
  `;
}
