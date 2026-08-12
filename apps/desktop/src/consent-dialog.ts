// Consent screen (design doc §8, §19 phase 6).
//
// Deny is the default: the dialog offers the lowest useful role first, states
// exactly what is being granted, and never pre-selects control. Keyboard
// reachable and screen-reader labelled — this screen has to pass an axe-core
// audit before release.

import { html, type TemplateResult } from 'lit-html';

import type { SessionStatus } from './session-status';

export type Role = 'view_only' | 'control_limited' | 'full_control';

async function grant(peer: string, role: Role): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_grant', { args: { peer, role } });
}

async function revoke(peer: string): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_revoke', { args: { peer } });
}

export function consentDialog(request: SessionStatus | undefined): TemplateResult {
  if (!request) {
    return html`
      <section class="consent" aria-live="polite">
        <h1>No pending requests</h1>
        <p>Nobody is asking to connect right now.</p>
      </section>
    `;
  }

  return html`
    <section class="consent" role="dialog" aria-modal="true" aria-labelledby="consent-title">
      <h1 id="consent-title">${request.peer_label} wants to connect</h1>
      <p>Granting view lets them see this screen. Input, clipboard, files and
        recording stay off until you enable each one separately.</p>
      <div class="consent-actions">
        <button type="button" @click=${() => void revoke(request.peer_label)}>Deny</button>
        <button type="button" @click=${() => void grant(request.peer_label, 'view_only')}>
          Allow view only
        </button>
        <button type="button" @click=${() => void grant(request.peer_label, 'full_control')}>
          Allow full control
        </button>
      </div>
    </section>
  `;
}
