// Invite creation and connect form (design doc §7).
//
// The host asks for a QR; the guest pastes/scans the resulting string back.
// Neither side's identity is typed in by a human — the ticket carries it.
//
// Split into two render functions because the visual design (PRODUCT.md,
// status-pill-preview.html) puts them in different places: the invite code
// lives in the sidebar, the connect form in the main panel.

import { html, type TemplateResult } from 'lit-html';
import QRCode from 'qrcode';

import type { Locale } from './i18n';
import { t } from './i18n';

const COPIED_FEEDBACK_MS = 1500;
const CONNECTING_DOT_INTERVAL_MS = 400;
const CONNECTING_DOT_MAX = 3;

let lastQr: { dataUrl: string; text: string } | undefined;
let creatingInvite = false;
let copied = false;
let connecting = false;
let connectingDots = 1;
let connectingTimer: ReturnType<typeof setInterval> | undefined;
let connectError: string | undefined;
let onChange: (() => void) | undefined;

/** Lets the caller (main.ts) re-render after async state above changes. */
export function onInviteStateChange(callback: () => void): void {
  onChange = callback;
}

function notify(): void {
  onChange?.();
}

async function createInvite(): Promise<void> {
  creatingInvite = true;
  notify();
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    const invite = await invoke<{ qr_string: string; expires_at: number }>('invite_create', {
      args: { role: 'view_only' },
    });
    const dataUrl = await QRCode.toDataURL(invite.qr_string);
    lastQr = { dataUrl, text: invite.qr_string };
  } finally {
    creatingInvite = false;
    notify();
  }
}

async function copyCode(): Promise<void> {
  if (!lastQr) {
    return;
  }
  try {
    await navigator.clipboard.writeText(lastQr.text);
    copied = true;
    notify();
    setTimeout(() => {
      copied = false;
      notify();
    }, COPIED_FEEDBACK_MS);
  } catch {
    // No clipboard permission: the code is still selectable/visible in the
    // code box, so there is nothing further to recover here.
  }
}

/**
 * Tauri command failures reject with the `IpcError { code, message }` shape
 * (see commands.rs), a plain object rather than an `Error` — `String()` on
 * that yields `[object Object]` instead of the message it carries.
 */
function describeError(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  if (typeof error === 'object' && error !== null && 'message' in error) {
    const message = (error as { message?: unknown }).message;
    if (typeof message === 'string') {
      return message;
    }
  }
  return String(error);
}

function startConnectingAnimation(): void {
  connectingDots = 1;
  clearInterval(connectingTimer);
  connectingTimer = setInterval(() => {
    connectingDots = (connectingDots % CONNECTING_DOT_MAX) + 1;
    notify();
  }, CONNECTING_DOT_INTERVAL_MS);
}

function stopConnectingAnimation(): void {
  clearInterval(connectingTimer);
  connectingTimer = undefined;
}

async function connect(ticket: string): Promise<void> {
  connectError = undefined;
  connecting = true;
  startConnectingAnimation();
  notify();
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    await invoke('invite_connect', { args: { ticket } });
  } catch (error) {
    connectError = describeError(error);
  } finally {
    connecting = false;
    stopConnectingAnimation();
    notify();
  }
}

/** Sidebar block: "Your invite code" label, code box + copy, or a create trigger. */
export function inviteCodePanel(locale: Locale): TemplateResult {
  if (!lastQr) {
    return html`
      <p class="field-label">${t(locale, 'sidebar.inviteLabel')}</p>
      <button
        type="button"
        class="create-btn"
        ?disabled=${creatingInvite}
        @click=${() => void createInvite()}
      >
        ${t(locale, 'invite.create')}
      </button>
    `;
  }

  return html`
    <p class="field-label">${t(locale, 'sidebar.inviteLabel')}</p>
    <button
      type="button"
      class="code-box"
      title=${lastQr.text}
      aria-label=${t(locale, 'sidebar.copyCode')}
      @click=${() => void copyCode()}
    >
      ${lastQr.text}
    </button>
    <button type="button" class="copy-btn" @click=${() => void copyCode()}>
      <svg width="13" height="13" viewBox="0 0 20 20" aria-hidden="true">
        <rect x="3" y="3" width="11" height="11" rx="2" stroke="currentColor" stroke-width="1.4" fill="none" />
        <rect x="6.5" y="6.5" width="11" height="11" rx="2" stroke="currentColor" stroke-width="1.4" fill="#fff" />
      </svg>
      ${copied ? t(locale, 'sidebar.copied') : t(locale, 'sidebar.copyCode')}
    </button>
    <button
      type="button"
      class="create-btn"
      ?disabled=${creatingInvite}
      @click=${() => void createInvite()}
    >
      ${t(locale, 'invite.refresh')}
    </button>
    <img class="invite-qr" alt=${t(locale, 'invite.qrAlt')} src=${lastQr.dataUrl} />
  `;
}

/** Main panel block: heading, subtext and the paste-a-ticket connect form. */
export function connectPanel(locale: Locale): TemplateResult {
  return html`
    <h1 class="panel-heading">${t(locale, 'panel.heading')}</h1>
    <p class="panel-subtext">${t(locale, 'panel.subtext')}</p>
    <form
      class="connect-row"
      @submit=${(event: SubmitEvent) => {
        event.preventDefault();
        const input = (event.target as HTMLFormElement).elements.namedItem('ticket') as HTMLInputElement;
        void connect(input.value);
      }}
    >
      <label class="sr-only" for="ticket-input">${t(locale, 'invite.connectLabel')}</label>
      <input
        id="ticket-input"
        name="ticket"
        type="text"
        placeholder=${t(locale, 'invite.connectPlaceholder')}
      />
      <button type="submit" class="connect-btn" ?disabled=${connecting}>
        ${connecting
          ? `${t(locale, 'invite.connecting')}${'.'.repeat(connectingDots)}`
          : t(locale, 'invite.connect')}
      </button>
    </form>
    ${connectError ? html`<p class="connect-error" role="alert">${connectError}</p>` : ''}
  `;
}
