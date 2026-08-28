// Host-side settings for unattended access (design doc §8; ADR 0033).
//
// This panel asks and shows; it never decides. Every question about the
// credentials is answered by `crates/core` through an IPC command, and the
// three things this file is allowed to know about them are the two booleans
// and the role `unattended_status` returns. The password is not one of them,
// in any form — there is no code path here that could receive it.
//
// The one exception, and it is deliberate: turning the second factor on
// returns the TOTP secret once, because an authenticator app cannot be set up
// without it. It is held in a module variable until the host dismisses the
// panel and is never re-fetchable.

import { html, nothing, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';
import type { Role } from './consent-dialog';

/** What `unattended_status` reports. Note what is absent (§13). */
export interface UnattendedStatus {
  enabled: boolean;
  totp_enabled: boolean;
  role: Role;
}

/** The one-time payload `unattended_set_totp` returns when turning it on. */
export interface TotpProvisioning {
  secret_base32: string;
  uri: string;
}

/** How long the "password saved" confirmation stays up. */
export const SAVED_FEEDBACK_MS = 3000;

type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>;

async function invoker(): Promise<Invoke> {
  const { invoke } = await import('@tauri-apps/api/core');
  return invoke as Invoke;
}

// Panel-local UI state. None of it is authority: every render reads the
// status the Rust side last reported, and these only decide what is on screen
// while the host is part-way through a change.
let provisioning: TotpProvisioning | undefined;
let savedAt = 0;
let errorMessage: string | undefined;
let busy = false;
let onChange: (() => void) | undefined;

/** Lets main.ts re-render after an async change here. */
export function onUnattendedStateChange(callback: () => void): void {
  onChange = callback;
}

function notify(): void {
  onChange?.();
}

/** Test seam: drops the transient state between cases. */
export function resetUnattendedPanel(): void {
  provisioning = undefined;
  savedAt = 0;
  errorMessage = undefined;
  busy = false;
}

/**
 * IPC errors arrive as `{ code, message }`, not as `Error` (commands.rs), so
 * `String(error)` would show `[object Object]`.
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

async function run(action: (invoke: Invoke) => Promise<void>, refresh: () => void): Promise<void> {
  busy = true;
  errorMessage = undefined;
  notify();
  try {
    await action(await invoker());
  } catch (error) {
    errorMessage = describeError(error);
  } finally {
    busy = false;
    refresh();
    notify();
  }
}

const ROLE_OPTIONS: readonly {
  role: Role;
  key: 'status.role.viewOnly' | 'status.role.controlLimited' | 'status.role.fullControl';
}[] = [
  { role: 'view_only', key: 'status.role.viewOnly' },
  { role: 'control_limited', key: 'status.role.controlLimited' },
  { role: 'full_control', key: 'status.role.fullControl' },
];

/**
 * The provisioning sheet, shown once after the second factor is turned on.
 *
 * The secret is rendered as selectable text rather than only as a link: this
 * has to work when the authenticator app is on a different machine and the
 * only way across is typing.
 */
function totpSheet(locale: Locale): TemplateResult | typeof nothing {
  if (!provisioning) {
    return nothing;
  }
  return html`
    <div class="totp-sheet" role="group" aria-label=${t(locale, 'unattended.totp.secretHeading')}>
      <h4>${t(locale, 'unattended.totp.secretHeading')}</h4>
      <p class="totp-note">${t(locale, 'unattended.totp.secretBody')}</p>
      <code class="totp-secret" data-testid="totp-secret">${provisioning.secret_base32}</code>
      <label class="totp-uri-label" for="totp-uri">${t(locale, 'unattended.totp.uriLabel')}</label>
      <input id="totp-uri" class="totp-uri" type="text" readonly .value=${provisioning.uri} />
      <button
        type="button"
        class="totp-done"
        @click=${() => {
          provisioning = undefined;
          notify();
        }}
      >
        ${t(locale, 'unattended.totp.done')}
      </button>
    </div>
  `;
}

/**
 * The unattended-access settings panel.
 *
 * `onRefresh` re-polls `unattended_status`: nothing here flips a switch
 * locally, so what the panel shows after a change is whatever the core says
 * it now holds.
 */
export function unattendedSettings(
  status: UnattendedStatus,
  locale: Locale,
  onRefresh: () => void = () => {},
): TemplateResult {
  const justSaved = Date.now() - savedAt < SAVED_FEEDBACK_MS;
  return html`
    <section class="unattended-panel" aria-labelledby="unattended-heading">
      <h3 id="unattended-heading">${t(locale, 'unattended.heading')}</h3>
      <p class="unattended-explain">${t(locale, 'unattended.explain')}</p>
      <p class="unattended-state">
        ${t(locale, status.enabled ? 'unattended.state.on' : 'unattended.state.off')}
      </p>

      <form
        class="unattended-password"
        @submit=${(event: SubmitEvent) => {
          event.preventDefault();
          const form = event.target as HTMLFormElement;
          const field = form.elements.namedItem('unattended-password') as HTMLInputElement;
          const password = field.value;
          if (password.length === 0) {
            return;
          }
          void run(async (invoke) => {
            await invoke('unattended_set_password', { args: { password } });
            // Cleared the moment it has been handed over: a password left in
            // a DOM node is a password on screen and in the next screenshot.
            field.value = '';
            savedAt = Date.now();
          }, onRefresh);
        }}
      >
        <label for="unattended-password">${t(locale, 'unattended.password.label')}</label>
        <input
          id="unattended-password"
          name="unattended-password"
          type="password"
          autocomplete="new-password"
          placeholder=${t(locale, 'unattended.password.placeholder')}
        />
        <button type="submit" class="unattended-save" ?disabled=${busy}>
          ${t(locale, status.enabled ? 'unattended.password.change' : 'unattended.password.set')}
        </button>
      </form>
      ${justSaved
        ? html`<p class="unattended-saved" role="status" data-testid="unattended-saved">
            ${t(locale, 'unattended.password.saved')}
          </p>`
        : ''}

      <label class="unattended-totp">
        <input
          type="checkbox"
          .checked=${status.totp_enabled}
          ?disabled=${busy || !status.enabled}
          aria-label=${t(locale, 'unattended.totp.label')}
          @change=${(event: Event) => {
            const enabled = (event.target as HTMLInputElement).checked;
            void run(async (invoke) => {
              const result = (await invoke('unattended_set_totp', {
                args: { enabled },
              })) as TotpProvisioning | null;
              provisioning = result ?? undefined;
            }, onRefresh);
          }}
        />
        <span>${t(locale, 'unattended.totp.label')}</span>
      </label>
      ${status.enabled ? '' : html`<p class="unattended-hint">${t(locale, 'unattended.needsTrust')}</p>`}
      ${totpSheet(locale)}

      <label class="unattended-role" for="unattended-role">${t(locale, 'unattended.role.label')}</label>
      <select
        id="unattended-role"
        ?disabled=${busy}
        @change=${(event: Event) => {
          const role = (event.target as HTMLSelectElement).value as Role;
          void run(async (invoke) => {
            await invoke('unattended_set_role', { args: { role } });
          }, onRefresh);
        }}
      >
        ${ROLE_OPTIONS.map(
          (option) => html`
            <option value=${option.role} ?selected=${status.role === option.role}>
              ${t(locale, option.key)}
            </option>
          `,
        )}
      </select>

      ${status.enabled
        ? html`
            <button
              type="button"
              class="unattended-disable"
              ?disabled=${busy}
              @click=${() => {
                // Turning it off deletes credentials, so it asks first — the
                // same standard the trust switch is held to.
                if (!globalThis.confirm(t(locale, 'unattended.disable.confirm'))) {
                  return;
                }
                void run(async (invoke) => {
                  await invoke('unattended_disable');
                  provisioning = undefined;
                }, onRefresh);
              }}
            >
              ${t(locale, 'unattended.disable')}
            </button>
          `
        : ''}
      ${errorMessage
        ? html`<p class="unattended-error" role="alert" data-testid="unattended-error">${errorMessage}</p>`
        : ''}
    </section>
  `;
}

/**
 * The line that cannot be dismissed while unattended access is on.
 *
 * §2 allows a host to answer with credentials instead of a click; it does not
 * allow that to be invisible. There is no close button here and no state that
 * could hide it: the only way to make it go away is to turn the feature off.
 * Lives in the sidebar (docs/bugs/05-settings-window.md, task 5; DECISIONS.md
 * D1) as one compact line — the full explanation is on `title` rather than in
 * the visible text.
 */
export function unattendedIndicator(
  status: UnattendedStatus,
  locale: Locale,
): TemplateResult | typeof nothing {
  if (!status.enabled) {
    return nothing;
  }
  return html`
    <p
      class="unattended-indicator"
      role="status"
      data-testid="unattended-indicator"
      title=${t(locale, 'unattended.indicator.title')}
    >
      ${t(locale, 'unattended.indicator')}
    </p>
  `;
}
