// Consent screen (design doc §8, §19 phase 6).
//
// Deny is the default: the dialog offers the lowest useful role first, states
// exactly what is being granted, and never pre-selects control. Keyboard
// reachable and screen-reader labelled — this screen has to pass an axe-core
// audit before release.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';
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

export function consentDialog(
  request: SessionStatus | undefined,
  locale: Locale,
): TemplateResult {
  if (!request) {
    return html`
      <section class="consent" aria-live="polite">
        <h1>${t(locale, 'consent.none.title')}</h1>
        <p>${t(locale, 'consent.none.body')}</p>
      </section>
    `;
  }

  return html`
    <section class="consent" role="dialog" aria-modal="true" aria-labelledby="consent-title">
      <h1 id="consent-title">${t(locale, 'consent.request.title', request.peer_label)}</h1>
      <p>${t(locale, 'consent.request.body')}</p>
      <div class="consent-actions">
        <button type="button" autofocus @click=${() => void revoke(request.peer_label)}>
          ${t(locale, 'consent.action.deny')}
        </button>
        <button type="button" @click=${() => void grant(request.peer_label, 'view_only')}>
          ${t(locale, 'consent.action.allowView')}
        </button>
        <button type="button" @click=${() => void grant(request.peer_label, 'full_control')}>
          ${t(locale, 'consent.action.allowFull')}
        </button>
      </div>
    </section>
  `;
}
