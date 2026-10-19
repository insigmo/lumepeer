// The license this install runs on (§12; ADR 0163).
//
// No license is the free plan, not an error state: the panel says which plan
// is in force and, where this build can reach a license server, takes the key
// a buyer was given. Everything the server says is decided in the core; this
// only shows it and passes the key along.

import { html, type TemplateResult } from 'lit-html';

import type { Locale, TranslationKey } from './i18n';
import { t } from './i18n';

/** What `license_status` reports. */
export interface LicenseStatus {
  plan: 'free' | 'trial' | 'pro' | 'team';
  licensed: boolean;
  seconds_left: number | null;
  offline: boolean;
  reason: string | null;
  activation_available: boolean;
}

/** How this panel reaches the core; injectable so tests need no Tauri. */
export interface LicenseCommands {
  status(): Promise<LicenseStatus>;
  activate(key: string): Promise<LicenseStatus>;
  deactivate(): Promise<LicenseStatus>;
}

export const tauriLicenseCommands: LicenseCommands = {
  async status() {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('license_status')) as LicenseStatus;
  },
  async activate(key) {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('license_activate', { args: { key } })) as LicenseStatus;
  },
  async deactivate() {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('license_deactivate')) as LicenseStatus;
  },
};

const PLAN_NAMES: Record<LicenseStatus['plan'], TranslationKey> = {
  free: 'license.plan.free',
  trial: 'license.plan.trial',
  pro: 'license.plan.pro',
  team: 'license.plan.team',
};

/** The IPC error codes `license_activate` answers with, by message. */
const ERRORS: Record<string, TranslationKey> = {
  LICENSE_REJECTED: 'license.error.rejected',
  LICENSE_UNREACHABLE: 'license.error.unreachable',
  LICENSE_INVALID: 'license.error.invalid',
};

interface State {
  status: LicenseStatus | null;
  requested: boolean;
  busy: boolean;
  error: TranslationKey | null;
}

const state: State = { status: null, requested: false, busy: false, error: null };

let onChange: (() => void) | undefined;

/** Lets main.ts re-render after an async change here. */
export function onLicenseStateChange(callback: () => void): void {
  onChange = callback;
}

/** Test seam: drops the panel's state between cases. */
export function resetLicenseSettings(): void {
  state.status = null;
  state.requested = false;
  state.busy = false;
  state.error = null;
}

function load(commands: LicenseCommands): void {
  if (state.requested) {
    return;
  }
  state.requested = true;
  void commands.status().then(
    (status) => {
      state.status = status;
      onChange?.();
    },
    (error: unknown) => {
      console.error('license status unavailable:', error);
    },
  );
}

function errorKey(error: unknown): TranslationKey {
  const code = (error as { code?: unknown } | null)?.code;
  return (typeof code === 'string' && ERRORS[code]) || 'license.error.failed';
}

async function run(action: () => Promise<LicenseStatus>, after?: () => void): Promise<void> {
  state.busy = true;
  state.error = null;
  onChange?.();
  try {
    state.status = await action();
    after?.();
  } catch (error: unknown) {
    state.error = errorKey(error);
  } finally {
    state.busy = false;
    onChange?.();
  }
}

/** The license panel of the About section. */
export function licenseSettings(
  locale: Locale,
  commands: LicenseCommands = tauriLicenseCommands,
): TemplateResult {
  load(commands);
  const status = state.status;
  const plan = t(locale, 'license.plan', t(locale, PLAN_NAMES[status?.plan ?? 'free']));

  return html`
    <section class="settings-group" data-testid="license-settings" aria-labelledby="license-heading">
      <h3 id="license-heading">${t(locale, 'license.heading')}</h3>
      <div class="setting-row">
        <span class="setting-text">
          <span class="setting-label" data-testid="license-plan">${plan}</span>
          ${status?.licensed && status.offline
            ? html`<span class="setting-note">${t(locale, 'license.offline')}</span>`
            : ''}
          ${status && !status.licensed && status.reason
            ? html`<span class="setting-note" data-testid="license-lapsed">${t(locale, 'license.lapsed')}</span>`
            : ''}
          ${status && !status.licensed && !status.activation_available
            ? html`<span class="setting-note">${t(locale, 'license.unavailable')}</span>`
            : ''}
        </span>
        ${status?.licensed
          ? html`<button
              type="button"
              class="btn"
              data-testid="license-deactivate"
              ?disabled=${state.busy}
              @click=${() => void run(() => commands.deactivate())}
            >
              ${t(locale, 'license.deactivate')}
            </button>`
          : ''}
      </div>
      ${status && !status.licensed && status.activation_available
        ? html`<form
            class="license-activate"
            @submit=${(event: SubmitEvent) => {
              event.preventDefault();
              const form = event.target as HTMLFormElement;
              const field = form.elements.namedItem('license-key') as HTMLInputElement;
              const key = field.value.trim();
              if (key.length === 0) {
                return;
              }
              void run(
                () => commands.activate(key),
                () => {
                  field.value = '';
                },
              );
            }}
          >
            <label for="license-key">${t(locale, 'license.key.label')}</label>
            <input
              id="license-key"
              name="license-key"
              type="text"
              autocomplete="off"
              spellcheck="false"
              placeholder=${t(locale, 'license.key.placeholder')}
            />
            <button type="submit" class="btn btn-primary" data-testid="license-activate" ?disabled=${state.busy}>
              ${t(locale, state.busy ? 'license.activating' : 'license.activate')}
            </button>
          </form>`
        : ''}
      ${state.error
        ? html`<p class="setting-note" role="alert" data-testid="license-error">${t(locale, state.error)}</p>`
        : ''}
    </section>
  `;
}
