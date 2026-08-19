// Invite creation and connect form (design doc §7).
//
// The host shows an invite code; the guest pastes it back. Neither side's
// identity is typed in by a human — the ticket carries it.
//
// Split into two render functions because the visual design (PRODUCT.md,
// status-pill-preview.html) puts them in different places: the invite code
// lives in the sidebar, the connect form in the main panel.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';

const COPIED_FEEDBACK_MS = 1500;
const CONNECTING_DOT_INTERVAL_MS = 400;
const CONNECTING_DOT_MAX = 3;

/**
 * How the Rust actor reports this node's own outgoing connect attempt
 * (`connect_status`, §21 punch-list item 6). `awaiting_consent` is the one
 * that matters here: the dial has landed and the far side is deciding.
 */
export type ConnectPhase = 'idle' | 'awaiting_consent' | 'connected' | 'denied' | 'failed';

let lastCode: string | undefined;
let creatingInvite = false;
let copied = false;
/** True from the moment `invite_connect` is invoked until it settles. */
let dialing = false;
/** Last phase the poll reported; drives the wait *after* the dial returns. */
let phase: ConnectPhase = 'idle';
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

/**
 * Whether a connect attempt is still outstanding.
 *
 * `invite_connect` returns as soon as the handshake lands, which is long
 * before the host user has decided anything — so the dial alone is not the
 * wait. The poll takes over from there and the button stays disabled for the
 * whole time, which is what stops a second request racing the first.
 */
export function isConnecting(): boolean {
  return dialing || phase === 'awaiting_consent';
}

/** Called by main.ts on every poll of `connect_status`. */
export function setConnectPhase(next: ConnectPhase): void {
  if (next === phase) {
    return;
  }
  const wasWaiting = isConnecting();
  phase = next;
  if (next === 'denied') {
    connectError = 'denied';
  } else if (next === 'failed') {
    connectError = 'failed';
  } else if (next === 'connected') {
    connectError = undefined;
  }
  if (wasWaiting !== isConnecting()) {
    syncConnectingAnimation();
  }
  notify();
}

async function invoker(): Promise<(cmd: string, args?: unknown) => Promise<unknown>> {
  const { invoke } = await import('@tauri-apps/api/core');
  return invoke as (cmd: string, args?: unknown) => Promise<unknown>;
}

async function createInvite(): Promise<void> {
  creatingInvite = true;
  notify();
  try {
    const invoke = await invoker();
    const invite = (await invoke('invite_create', { args: { role: 'view_only' } })) as {
      code: string;
    };
    lastCode = invite.code;
  } finally {
    creatingInvite = false;
    notify();
  }
}

async function copyCode(): Promise<void> {
  if (!lastCode) {
    return;
  }
  try {
    await navigator.clipboard.writeText(lastCode);
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

function syncConnectingAnimation(): void {
  if (isConnecting()) {
    connectingDots = 1;
    clearInterval(connectingTimer);
    connectingTimer = setInterval(() => {
      connectingDots = (connectingDots % CONNECTING_DOT_MAX) + 1;
      notify();
    }, CONNECTING_DOT_INTERVAL_MS);
    return;
  }
  clearInterval(connectingTimer);
  connectingTimer = undefined;
}

/** Runs one outgoing attempt, whatever supplied the target. */
async function attempt(command: string, args: unknown): Promise<void> {
  if (isConnecting()) {
    return;
  }
  connectError = undefined;
  dialing = true;
  phase = 'idle';
  syncConnectingAnimation();
  notify();
  try {
    const invoke = await invoker();
    await invoke(command, args);
    // The handshake landed; the host user has not decided yet. Assume the wait
    // rather than waiting for the next poll, so the button never flickers back
    // to enabled in between.
    phase = 'awaiting_consent';
  } catch (error) {
    connectError = describeError(error);
    phase = 'failed';
  } finally {
    dialing = false;
    syncConnectingAnimation();
    notify();
  }
}

async function connect(ticket: string): Promise<void> {
  await attempt('invite_connect', { args: { ticket } });
}

/** Dials a host the app already remembers, by its history label (§21 item 5). */
export async function reconnect(peer: string): Promise<void> {
  await attempt('history_connect', { args: { peer } });
}

/** The message under the connect form, if there is one to show. */
function errorText(locale: Locale): string | undefined {
  if (connectError === 'denied') {
    return t(locale, 'invite.denied');
  }
  if (connectError === 'failed') {
    return t(locale, 'invite.failed');
  }
  return connectError;
}

/** Sidebar block: "Your invite code" label, code box + copy, or a create trigger. */
export function inviteCodePanel(locale: Locale): TemplateResult {
  if (!lastCode) {
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
      title=${lastCode}
      aria-label=${t(locale, 'sidebar.copyCode')}
      @click=${() => void copyCode()}
    >
      ${lastCode}
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
  `;
}

/** Main panel block: heading, subtext and the paste-a-code connect form. */
export function connectPanel(locale: Locale): TemplateResult {
  const waiting = isConnecting();
  const message = errorText(locale);
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
      <button type="submit" class="connect-btn" ?disabled=${waiting}>
        ${waiting
          ? `${t(locale, 'invite.connecting')}${'.'.repeat(connectingDots)}`
          : t(locale, 'invite.connect')}
      </button>
    </form>
    ${message ? html`<p class="connect-error" role="alert">${message}</p>` : ''}
  `;
}
