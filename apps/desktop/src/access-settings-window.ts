// The elevated access-settings window (ADR 0169 §5).
//
// Where the Lumepeer service hosts this machine, the client the person runs
// only reads who may get in. Changing it starts this app again elevated with
// `--access-settings`: one UAC prompt, and this page — the device password,
// the second factor, the unattended role, the sign-in screen and the trusted
// devices, nothing else. The process behind it has no actor, no endpoint and
// no guest window; the commands this page calls are the main window's own
// `unattended_*` and `address_book_*`, which act on the protected stores and
// then tell the session host to read them again.
//
// Same shape as the other single-purpose pages (`hostbar.ts`, `agentbar.ts`):
// a render function over state polled from Rust, and nothing decided here.

import { html, render, type TemplateResult } from 'lit-html';

import { addressBook, onAddressBookStateChange, type AddressBookEntry } from './address-book';
import { dirOf, resolveLocale, t, type Locale } from './i18n';
import { followTheme } from './theme';
import {
  onUnattendedStateChange,
  unattendedSettings,
  type UnattendedStatus,
} from './unattended-settings';

/** How often the page re-reads the stores while it is open. */
export const ACCESS_POLL_MS = 2000;

/** What the page shows, as Rust last reported it. */
export interface AccessState {
  unattended: UnattendedStatus;
  devices: AddressBookEntry[];
}

/** The page itself; exported for the tests. */
export function accessSettingsView(
  state: AccessState,
  locale: Locale,
  onRefresh: () => void,
): TemplateResult {
  return html`
    <h2>${t(locale, 'access.heading')}</h2>
    <p class="access-explain">${t(locale, 'access.explain')}</p>
    ${unattendedSettings(state.unattended, locale, onRefresh)}
    ${addressBook(state.devices, locale, onRefresh)}
  `;
}

const root = document.querySelector<HTMLElement>('#access');
const locale: Locale = resolveLocale(navigator);
// Starts from the safest reading, as the main window does: off, nobody
// trusted, until Rust says otherwise.
let state: AccessState = {
  unattended: { enabled: false, totp_enabled: false, role: 'view_only' },
  devices: [],
};

function renderNow(): void {
  if (!root) {
    return;
  }
  document.documentElement.lang = locale;
  document.documentElement.dir = dirOf(locale);
  document.title = t(locale, 'access.heading');
  render(
    accessSettingsView(state, locale, () => void refresh()),
    root,
  );
}

async function refresh(): Promise<void> {
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    const [unattended, devices] = await Promise.all([
      invoke<UnattendedStatus>('unattended_status'),
      invoke<AddressBookEntry[]>('address_book_list'),
    ]);
    state = { unattended, devices };
  } catch (error) {
    console.error('access settings refresh failed:', error);
  }
  renderNow();
}

if (root) {
  followTheme();
  onUnattendedStateChange(renderNow);
  onAddressBookStateChange(renderNow);
  renderNow();
  void refresh();
  setInterval(() => void refresh(), ACCESS_POLL_MS);
}
