// Invite creation and connect form (design doc §7).
//
// The host asks for a QR; the guest pastes/scans the resulting string back.
// Neither side's identity is typed in by a human — the ticket carries it.

import { html, type TemplateResult } from 'lit-html';
import QRCode from 'qrcode';

import type { Locale } from './i18n';
import { t } from './i18n';

let lastQr: { dataUrl: string; text: string } | undefined;
let connectError: string | undefined;

async function createInvite(): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  const invite = await invoke<{ qr_string: string; expires_at: number }>('invite_create', {
    args: { role: 'view_only' },
  });
  const dataUrl = await QRCode.toDataURL(invite.qr_string);
  lastQr = { dataUrl, text: invite.qr_string };
}

async function connect(ticket: string): Promise<void> {
  connectError = undefined;
  const { invoke } = await import('@tauri-apps/api/core');
  try {
    await invoke('invite_connect', { args: { ticket } });
  } catch (error) {
    connectError = error instanceof Error ? error.message : String(error);
  }
}

export function inviteView(locale: Locale): TemplateResult {
  return html`
    <section class="invite">
      <h2>${t(locale, 'invite.heading')}</h2>
      <button type="button" @click=${() => void createInvite()}>
        ${t(locale, 'invite.create')}
      </button>
      ${lastQr
        ? html`<img alt=${t(locale, 'invite.qrAlt')} src=${lastQr.dataUrl} />
            <p>${lastQr.text}</p>`
        : ''}
      <form
        @submit=${(event: SubmitEvent) => {
          event.preventDefault();
          const input = (event.target as HTMLFormElement).elements.namedItem(
            'ticket',
          ) as HTMLInputElement;
          void connect(input.value);
        }}
      >
        <label for="ticket-input">${t(locale, 'invite.connectLabel')}</label>
        <input id="ticket-input" name="ticket" type="text" />
        <button type="submit">${t(locale, 'invite.connect')}</button>
      </form>
      ${connectError ? html`<p role="alert">${connectError}</p>` : ''}
    </section>
  `;
}
