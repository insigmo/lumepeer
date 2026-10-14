// The macOS permissions screen.
//
// A Mac lets Lumepeer show its screen, take a guest's mouse and keyboard and
// send its microphone only once the person at it has switched each of those
// on in System Settings; no installer can do it for them. Asked one at a time
// at the moment a session first needed each, the first session on a new Mac
// never worked: the Screen Recording prompt landed in the middle of a guest's
// connection, Accessibility at its first click. This screen asks for all
// three before anyone connects, and keeps saying which are still off
// (ADR 0158).
//
// It opens by itself on the first start, and again whenever a permission it
// saw on is off — an update signed differently loses them. The person can
// close it with some still off (a Mac used only to reach other computers
// needs none of the host's two), and reopen it from the settings. While open
// it reads the three again every second and a half, so a switch flipped in
// System Settings shows here without a click.

import { html, nothing, type TemplateResult } from 'lit-html';

import type { Locale, TranslationKey } from './i18n';
import { t } from './i18n';

/** One permission, in the order the screen lists them. */
export const MAC_PERMISSIONS = ['screen_recording', 'accessibility', 'microphone'] as const;
export type MacPermission = (typeof MAC_PERMISSIONS)[number];

/** Where one stands; `denied` is changed only in System Settings. */
export type MacPermissionState = 'granted' | 'not_granted' | 'denied';

/** What `mac_permissions` hands back; `null` off macOS. */
export type MacPermissionsDto = Record<MacPermission, MacPermissionState>;

/** How this screen talks to Tauri; injectable for tests. */
export interface MacPermissionsCommands {
  read(): Promise<MacPermissionsDto | null>;
  request(permission: MacPermission): Promise<void>;
}

export const tauriMacPermissionsCommands: MacPermissionsCommands = {
  async read() {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<MacPermissionsDto | null>('mac_permissions');
  },
  async request(permission) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('mac_permission_request', { args: { permission } });
  },
};

/** How often an open screen reads the permissions again. */
const POLL_MS = 1500;

/**
 * Where the permissions on when the screen was last closed are kept, so a
 * later start can tell one that was lost from one never given.
 */
const GRANTED_KEY = 'lumepeer.macPermissions.granted';

const TEXT: Record<MacPermission, { title: TranslationKey; body: TranslationKey }> = {
  screen_recording: { title: 'macPermissions.screen.title', body: 'macPermissions.screen.body' },
  accessibility: {
    title: 'macPermissions.accessibility.title',
    body: 'macPermissions.accessibility.body',
  },
  microphone: { title: 'macPermissions.microphone.title', body: 'macPermissions.microphone.body' },
};

const STATE_TEXT: Record<MacPermissionState, TranslationKey> = {
  granted: 'macPermissions.state.granted',
  not_granted: 'macPermissions.state.notGranted',
  denied: 'macPermissions.state.denied',
};

let commands: MacPermissionsCommands = tauriMacPermissionsCommands;
let current: MacPermissionsDto | null = null;
let open = false;
let timer: ReturnType<typeof setInterval> | undefined;
let onChange: (() => void) | undefined;

/** Lets main.ts re-render after this module's state changes. */
export function onMacPermissionsStateChange(callback: () => void): void {
  onChange = callback;
}

/** Test seam: drops all state between cases. */
export function resetMacPermissions(): void {
  if (timer !== undefined) {
    clearInterval(timer);
  }
  timer = undefined;
  commands = tauriMacPermissionsCommands;
  current = null;
  open = false;
  onChange = undefined;
}

/** The permissions remembered as on, or `null` before the screen ever closed. */
function rememberedGranted(): MacPermission[] | null {
  try {
    const raw = localStorage.getItem(GRANTED_KEY);
    if (raw === null) {
      return null;
    }
    return raw === '' ? [] : (raw.split(',') as MacPermission[]);
  } catch {
    return null;
  }
}

function rememberGranted(permissions: MacPermissionsDto): void {
  try {
    localStorage.setItem(
      GRANTED_KEY,
      MAC_PERMISSIONS.filter((permission) => permissions[permission] === 'granted').join(','),
    );
  } catch {
    // A webview without storage asks again on the next start, which is the
    // safe direction.
  }
}

/**
 * Whether the screen opens by itself: something is off, and either it has
 * never been shown on this Mac or a permission it saw on has gone.
 */
export function shouldOpenAtStart(
  permissions: MacPermissionsDto,
  granted: readonly MacPermission[] | null,
): boolean {
  const missing = MAC_PERMISSIONS.filter((permission) => permissions[permission] !== 'granted');
  if (missing.length === 0) {
    return false;
  }
  return granted === null || granted.some((permission) => permissions[permission] !== 'granted');
}

async function refresh(): Promise<void> {
  try {
    current = await commands.read();
  } catch (error: unknown) {
    console.error('mac_permissions failed:', error);
  }
  onChange?.();
}

function startPolling(): void {
  if (timer === undefined) {
    timer = setInterval(() => void refresh(), POLL_MS);
  }
}

function stopPolling(): void {
  if (timer !== undefined) {
    clearInterval(timer);
  }
  timer = undefined;
}

/**
 * Reads the permissions once at start, and opens the screen if it should.
 * Does nothing visible off macOS, where the answer is `null`.
 */
export async function startMacPermissions(
  source: MacPermissionsCommands = tauriMacPermissionsCommands,
): Promise<void> {
  commands = source;
  await refresh();
  if (current && shouldOpenAtStart(current, rememberedGranted())) {
    openMacPermissions();
  }
}

/** Whether this machine has macOS permissions to show at all. */
export function hasMacPermissions(): boolean {
  return current !== null;
}

/** Opens the screen, from the start or from the settings. */
export function openMacPermissions(): void {
  if (current === null) {
    return;
  }
  open = true;
  startPolling();
  onChange?.();
  queueMicrotask(() => {
    document.getElementById('mac-permissions-done')?.focus();
  });
}

/** Closes the screen, remembering what was on at that moment. */
export function closeMacPermissions(): void {
  if (!open) {
    return;
  }
  open = false;
  stopPolling();
  if (current) {
    rememberGranted(current);
  }
  onChange?.();
}

function ask(permission: MacPermission): void {
  void commands.request(permission).then(
    () => refresh(),
    (error: unknown) => console.error('mac_permission_request failed:', error),
  );
}

function row(locale: Locale, permission: MacPermission, state: MacPermissionState): TemplateResult {
  const text = TEXT[permission];
  return html`
    <li class="mac-perm" data-testid=${`mac-permission-${permission}`} data-state=${state}>
      <span class="mac-perm-text">
        <span class="mac-perm-title">${t(locale, text.title)}</span>
        <span class="mac-perm-body">${t(locale, text.body)}</span>
      </span>
      <span class="mac-perm-state">${t(locale, STATE_TEXT[state])}</span>
      ${state === 'granted'
        ? nothing
        : html`<button
            type="button"
            class="btn"
            data-testid=${`mac-permission-ask-${permission}`}
            @click=${() => ask(permission)}
          >
            ${t(locale, state === 'denied' ? 'macPermissions.openSettings' : 'macPermissions.allow')}
          </button>`}
    </li>
  `;
}

/** The screen, when it is open. */
export function macPermissionsView(locale: Locale): TemplateResult | typeof nothing {
  if (!open || current === null) {
    return nothing;
  }
  const permissions = current;
  const all = MAC_PERMISSIONS.every((permission) => permissions[permission] === 'granted');
  return html`
    <div
      class="settings-backdrop"
      @keydown=${(event: KeyboardEvent) => {
        if (event.key === 'Escape') {
          event.preventDefault();
          closeMacPermissions();
        }
      }}
    >
      <section
        class="mac-perms"
        role="dialog"
        aria-modal="true"
        aria-labelledby="mac-permissions-heading"
        data-testid="mac-permissions"
      >
        <h2 id="mac-permissions-heading">${t(locale, 'macPermissions.heading')}</h2>
        <p class="mac-perms-lead">${t(locale, 'macPermissions.lead')}</p>
        <ul class="mac-perms-list">
          ${MAC_PERMISSIONS.map((permission) => row(locale, permission, permissions[permission]))}
        </ul>
        <p class="mac-perms-note">${t(locale, 'macPermissions.restartNote')}</p>
        <div class="mac-perms-actions">
          <button
            id="mac-permissions-done"
            type="button"
            class=${all ? 'btn btn-primary' : 'btn'}
            data-testid="mac-permissions-done"
            @click=${() => closeMacPermissions()}
          >
            ${t(locale, all ? 'macPermissions.done' : 'macPermissions.later')}
          </button>
        </div>
      </section>
    </div>
  `;
}

/** The settings row that reopens the screen; nothing off macOS. */
export function macPermissionsSettingsRow(locale: Locale): TemplateResult | typeof nothing {
  if (current === null) {
    return nothing;
  }
  return html`
    <section class="settings-group" aria-labelledby="mac-permissions-settings-heading">
      <h3 id="mac-permissions-settings-heading">${t(locale, 'macPermissions.heading')}</h3>
      <div class="setting-row">
        <span class="setting-text">
          <span class="setting-note">${t(locale, 'macPermissions.settingsNote')}</span>
        </span>
        <button
          type="button"
          class="btn"
          data-testid="mac-permissions-open"
          @click=${() => openMacPermissions()}
        >
          ${t(locale, 'macPermissions.settingsOpen')}
        </button>
      </div>
    </section>
  `;
}
