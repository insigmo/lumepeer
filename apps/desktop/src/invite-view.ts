// Invite creation and connect form (design doc §7).
//
// The host shows an invite code; the guest pastes it back. Neither side's
// identity is typed in by a human — the ticket carries it.
//
// Split into two render functions because the visual design (PRODUCT.md,
// status-pill-preview.html) puts them in different places: the invite code
// lives in the sidebar, the connect form in the main panel.

import { html, type TemplateResult } from 'lit-html';

import type { Locale, TranslationKey } from './i18n';
import { t } from './i18n';

const COPIED_FEEDBACK_MS = 1500;
const CONNECTING_DOT_INTERVAL_MS = 400;
const CONNECTING_DOT_MAX = 3;

/**
 * How the Rust actor reports this node's own outgoing connect attempt
 * (`connect_status`, §21 punch-list item 6). Two of these are waits, not
 * outcomes: `dialing` is this node still trying to reach the host, and
 * `awaiting_consent` is the dial having landed with the far side deciding.
 */
export type ConnectPhase =
  | 'idle'
  | 'dialing'
  | 'awaiting_consent'
  | 'awaiting_credentials'
  | 'connected'
  | 'denied'
  | 'failed';

/**
 * §18 code the actor attaches to a failed attempt, so the form can say which
 * failure it was instead of one generic sentence (ADR 0027). An unrecognised
 * code falls back to the generic wording rather than showing the raw code.
 */
const FAILURE_TEXT: Record<string, TranslationKey> = {
  DIAL_FAILED: 'invite.unreachable',
  BAD_TICKET: 'invite.badTicket',
  OFFLINE: 'invite.offline',
  INCOMPATIBLE_VERSION: 'invite.versionMismatch',
  TRANSPORT_LOST: 'invite.failed',
};

/**
 * What a refused device credential says (§8, §18; ADR 0033).
 *
 * The host tells this side which factor to retype and nothing more — never
 * how close a guess was, and never that one factor was right while the other
 * was not. These four are the whole vocabulary.
 */
const CREDENTIAL_ERROR_TEXT: Record<string, TranslationKey> = {
  UNATTENDED_BAD_PASSWORD: 'creds.badPassword',
  UNATTENDED_BAD_CODE: 'creds.badCode',
  UNATTENDED_UNAVAILABLE: 'creds.unavailable',
};

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
  return (
    dialing ||
    phase === 'dialing' ||
    phase === 'awaiting_consent' ||
    phase === 'awaiting_credentials'
  );
}

/**
 * Whether the host asked for device credentials and is waiting on this user
 * (§8; ADR 0033).
 */
export function isAwaitingCredentials(): boolean {
  return phase === 'awaiting_credentials';
}

/**
 * Called by main.ts on every poll of `connect_status`.
 *
 * `code` is the §18 classification of a failure, present only with
 * `phase: 'failed'`. It exists because the dial no longer runs inside the IPC
 * call and so can no longer reject it: without this every transport problem
 * would reach the user as the same sentence (ADR 0027).
 */
export function setConnectPhase(
  next: ConnectPhase,
  code?: string | null,
  codeRequired = false,
  retrySecs?: number | null,
): void {
  const nextError = failureKey(next, code);
  const unchanged =
    next === phase &&
    nextError === connectError &&
    codeRequired === credentialCodeRequired &&
    (retrySecs ?? undefined) === credentialRetrySecs &&
    (code ?? undefined) === credentialCode;
  credentialCodeRequired = codeRequired;
  credentialRetrySecs = retrySecs ?? undefined;
  credentialCode = next === 'awaiting_credentials' ? (code ?? undefined) : undefined;
  if (submitting && next !== 'awaiting_credentials') {
    // The answer landed, refused or not: let a later attempt submit again.
    submitting = false;
  }
  if (unchanged) {
    return;
  }
  const wasWaiting = isConnecting();
  phase = next;
  if (nextError !== undefined || next === 'connected' || isConnecting()) {
    connectError = nextError;
  }
  if (wasWaiting !== isConnecting()) {
    syncConnectingAnimation();
  }
  notify();
}

/** Whether the host's challenge asked for a one-time code as well. */
let credentialCodeRequired = false;
/** Seconds the host said to wait, after a lockout. */
let credentialRetrySecs: number | undefined;
/** §18 code of the last refused credential, while the form is still up. */
let credentialCode: string | undefined;
/** True from the moment credentials are submitted until an answer lands. */
let submitting = false;

/** The message key a phase deserves, or `undefined` when it deserves none. */
function failureKey(next: ConnectPhase, code?: string | null): TranslationKey | undefined {
  if (next === 'denied') {
    return 'invite.denied';
  }
  if (next === 'failed') {
    return (code && FAILURE_TEXT[code]) || 'invite.failed';
  }
  return undefined;
}

/**
 * The credential form the host's challenge puts up (§8; ADR 0033).
 *
 * The password lives in the field and in the one IPC call that carries it. It
 * is cleared as soon as that call is made, and nothing in this module keeps a
 * copy — a wrong password means retyping it, which is the correct trade.
 */
export function credentialsPanel(locale: Locale): TemplateResult {
  const errorKey = credentialCode ? CREDENTIAL_ERROR_TEXT[credentialCode] : undefined;
  const message =
    credentialCode === 'UNATTENDED_LOCKED_OUT'
      ? t(locale, 'creds.lockedOut', String(credentialRetrySecs ?? 0))
      : errorKey
        ? t(locale, errorKey)
        : undefined;
  return html`
    <section class="credentials-panel" aria-labelledby="credentials-heading">
      <h2 id="credentials-heading">${t(locale, 'creds.heading')}</h2>
      <p class="credentials-body">${t(locale, 'creds.body')}</p>
      <form
        class="credentials-form"
        @submit=${(event: SubmitEvent) => {
          event.preventDefault();
          const form = event.target as HTMLFormElement;
          const passwordField = form.elements.namedItem('device-password') as HTMLInputElement;
          const codeField = form.elements.namedItem('device-code') as HTMLInputElement | null;
          const password = passwordField.value;
          const code = codeField?.value ?? '';
          passwordField.value = '';
          if (codeField) {
            codeField.value = '';
          }
          void submitCredentials(password, code);
        }}
      >
        <label for="device-password">${t(locale, 'creds.password.label')}</label>
        <input
          id="device-password"
          name="device-password"
          type="password"
          autocomplete="current-password"
          placeholder=${t(locale, 'creds.password.placeholder')}
        />
        ${credentialCodeRequired
          ? html`
              <label for="device-code">${t(locale, 'creds.code.label')}</label>
              <input
                id="device-code"
                name="device-code"
                type="text"
                inputmode="numeric"
                autocomplete="one-time-code"
                placeholder=${t(locale, 'creds.code.placeholder')}
              />
            `
          : ''}
        <button type="submit" class="credentials-submit" ?disabled=${submitting}>
          ${submitting ? t(locale, 'creds.checking') : t(locale, 'creds.submit')}
        </button>
      </form>
      ${message
        ? html`<p class="credentials-error" role="alert" data-testid="credentials-error">${message}</p>`
        : ''}
    </section>
  `;
}

async function submitCredentials(password: string, code: string): Promise<void> {
  submitting = true;
  credentialCode = undefined;
  notify();
  try {
    const invoke = await invoker();
    await invoke('unattended_submit', {
      args: { password, code: credentialCodeRequired ? code : null },
    });
  } catch (error) {
    // The call itself failed — the session went away, or nothing was waiting
    // on a challenge. Whether the *credentials* were right never arrives this
    // way; it comes back on the wire and through the next status poll.
    submitting = false;
    console.error('unattended_submit failed:', describeError(error));
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
    // The attempt is under way — the dial runs off the actor loop and reports
    // through `connect_status` from here (ADR 0027). Assume the wait rather
    // than waiting for the next poll, so the button never flickers back to
    // enabled in between.
    phase = 'dialing';
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
  if (connectError === undefined) {
    return undefined;
  }
  // A key set from a phase is localized here; anything else is the message a
  // synchronously refused IPC call already carried.
  return connectError.startsWith('invite.')
    ? t(locale, connectError as TranslationKey)
    : connectError;
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
