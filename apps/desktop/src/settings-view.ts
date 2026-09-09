// Settings screen (docs/bugs/05-settings-window.md).
//
// A modal overlay over the main window rather than a second Tauri window
// (DECISIONS.md D9/"Решение: окно или оверлей"): cheap, needs nothing added
// to `capabilities/`, and the panels it hosts are unchanged — this module
// only decides where they render, never what they do.
//
// Pure render function plus module state (`open`, `onChange`), the same
// shape as `invite-view.ts` and `unattended-settings.ts`.

import { html, nothing, type TemplateResult } from 'lit-html';

import type { AddressBookEntry } from './address-book';
import { addressBook } from './address-book';
import type { AuditCommands } from './audit-log';
import { auditPanel } from './audit-log';
import { inviteRefreshPanel } from './invite-view';
import type { Locale, TranslationKey } from './i18n';
import { t } from './i18n';
import type { RecordingEntry, RecordingsCommands } from './recordings';
import { recordingsPanel } from './recordings';
import type { SystemCommands } from './system-settings';
import { systemSettings } from './system-settings';
import type { UnattendedStatus } from './unattended-settings';
import { unattendedSettings } from './unattended-settings';

/** The three sections the panels are grouped into. */
export type SettingsTab = 'system' | 'access' | 'recordings';

/**
 * The tabs, in order, with the label key each one carries.
 *
 * A flat list of six panels had the address book, the unattended password,
 * invite revocation, recordings, the audit log and the system switches in one
 * scroll, which is what made finding any of them a hunt. The grouping is by
 * the question being answered: what this machine is and who it lets in, how a
 * trusted device authenticates, and what has been kept about sessions that
 * already happened.
 *
 * `system` is first and holds what used to be a separate Devices tab. The two
 * were the same subject read twice — this machine's own settings, and the
 * devices this machine deals with — and the split only decided which of them
 * an operator had to click past first.
 */
const TABS: readonly { readonly id: SettingsTab; readonly label: TranslationKey }[] = [
  { id: 'system', label: 'settings.tab.system' },
  { id: 'access', label: 'settings.tab.access' },
  { id: 'recordings', label: 'settings.tab.recordings' },
];

let open = false;
/** Which section is showing. Reset on close, so opening starts predictably. */
let tab: SettingsTab = 'system';
let onChange: (() => void) | undefined;
/** The element to return focus to on close: whatever had focus when opened. */
let trigger: HTMLElement | null = null;
/** Bumped on every open, so the close button is focused once per open. */
let focusToken = 0;
let focusedToken = -1;

/** Lets main.ts re-render after this module's state changes. */
export function onSettingsStateChange(callback: () => void): void {
  onChange = callback;
}

function notify(): void {
  onChange?.();
}

export function isSettingsOpen(): boolean {
  return open;
}

/** Opens the settings screen, remembering what to return focus to. */
export function openSettings(): void {
  if (open) {
    return;
  }
  trigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  open = true;
  tab = 'system';
  focusToken += 1;
  notify();
}

/** Closes the settings screen and returns focus to the button that opened it. */
export function closeSettings(): void {
  if (!open) {
    return;
  }
  open = false;
  notify();
  trigger?.focus();
  trigger = null;
}

/** Switches section and re-renders. */
export function selectTab(next: SettingsTab): void {
  if (tab === next) {
    return;
  }
  tab = next;
  notify();
}

/** Which section is showing; exported for the tests. */
export function activeTab(): SettingsTab {
  return tab;
}

/**
 * Arrow-key movement along the tab strip, as the WAI-ARIA tabs pattern wants
 * it: only the selected tab is in the tab order, so the arrows are the only
 * way a keyboard reaches the others. Direction follows the document, so it
 * still reads left-to-right in Arabic's mirrored layout.
 */
function onTabKey(event: KeyboardEvent, id: SettingsTab): void {
  const back = document.documentElement.dir === 'rtl' ? 'ArrowRight' : 'ArrowLeft';
  const forward = back === 'ArrowLeft' ? 'ArrowRight' : 'ArrowLeft';
  const step = event.key === forward ? 1 : event.key === back ? -1 : 0;
  if (step === 0) {
    return;
  }
  event.preventDefault();
  const at = TABS.findIndex((entry) => entry.id === id);
  const next = TABS[(at + step + TABS.length) % TABS.length]?.id ?? id;
  selectTab(next);
  queueMicrotask(() => {
    document.getElementById(`settings-tab-${next}`)?.focus();
  });
}

/** Test seam: drops transient state (open, remembered focus) between cases. */
export function resetSettingsView(): void {
  open = false;
  tab = 'system';
  trigger = null;
  focusToken = 0;
  focusedToken = -1;
}

export interface SettingsPanels {
  locale: Locale;
  unattended: UnattendedStatus;
  savedDevices: AddressBookEntry[];
  recordings: RecordingEntry[];
  recordingsCommands: RecordingsCommands;
  auditCommands: AuditCommands;
  systemCommands: SystemCommands;
  onRefresh: () => void;
  /** Applies a manual language choice; undefined leaves the picker inert. */
  onLocaleChange?: (locale: Locale) => void;
}

/** The panels belonging to one section. */
function section(panels: SettingsPanels): TemplateResult {
  const { locale } = panels;
  switch (tab) {
    case 'system':
      // How the app itself behaves, who this machine lets in, and the code it
      // hands out to invite them.
      return html`
        ${systemSettings(locale, panels.systemCommands, panels.onLocaleChange)}
        ${addressBook(panels.savedDevices, locale, panels.onRefresh)}
        ${inviteRefreshPanel(locale)}
      `;
    case 'access':
      // How a device that is already trusted proves it is itself.
      return html`${unattendedSettings(panels.unattended, locale, panels.onRefresh)}`;
    case 'recordings':
      // What this machine has kept about sessions that already happened.
      return html`
        ${recordingsPanel(panels.recordings, locale, panels.recordingsCommands, panels.onRefresh)}
        ${auditPanel(locale, panels.auditCommands)}
      `;
  }
}

/**
 * The settings screen: this device, the address book, invite revocation,
 * unattended access, recordings and the audit log, moved here from the main
 * panel (DECISIONS.md D9) and grouped into the three sections of [`TABS`].
 * None of these panels are rewritten — each keeps its own render function and
 * arguments; this module only decides which of them are on screen.
 */
export function settingsView(panels: SettingsPanels): TemplateResult | typeof nothing {
  if (!open) {
    return nothing;
  }
  if (focusToken !== focusedToken) {
    focusedToken = focusToken;
    queueMicrotask(() => {
      document.getElementById('settings-close')?.focus();
    });
  }
  const { locale } = panels;
  return html`
    <div
      class="settings-backdrop"
      @keydown=${(event: KeyboardEvent) => {
        if (event.key === 'Escape') {
          event.preventDefault();
          closeSettings();
        }
      }}
    >
      <section class="settings-panel" role="dialog" aria-modal="true" aria-labelledby="settings-heading">
        <div class="settings-head">
          <h2 id="settings-heading">${t(locale, 'settings.heading')}</h2>
          <button
            id="settings-close"
            type="button"
            class="settings-close"
            aria-label=${t(locale, 'settings.close')}
            @click=${() => closeSettings()}
          >
            ×
          </button>
        </div>
        <div class="settings-tabs" role="tablist" aria-label=${t(locale, 'settings.tabs.label')}>
          ${TABS.map(
            (entry) => html`
              <button
                type="button"
                role="tab"
                id=${`settings-tab-${entry.id}`}
                class=${entry.id === tab ? 'settings-tab settings-tab-active' : 'settings-tab'}
                aria-selected=${entry.id === tab}
                aria-controls="settings-tabpanel"
                tabindex=${entry.id === tab ? 0 : -1}
                @keydown=${(event: KeyboardEvent) => onTabKey(event, entry.id)}
                @click=${() => selectTab(entry.id)}
              >
                ${t(locale, entry.label)}
              </button>
            `,
          )}
        </div>
        <div
          class="settings-body"
          id="settings-tabpanel"
          role="tabpanel"
          aria-labelledby=${`settings-tab-${tab}`}
        >
          ${section(panels)}
        </div>
      </section>
    </div>
  `;
}
