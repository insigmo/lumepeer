// Settings screen (docs/bugs/05-settings-window.md).
//
// A modal overlay over the main window rather than a second Tauri window
// (DECISIONS.md D9/"Решение: окно или оверлей"): cheap, needs nothing added
// to `capabilities/`, and the panels it hosts are unchanged — this module
// only decides where they render, never what they do.
//
// Pure render function plus module state (`open`, `onChange`), the same
// shape as `invite-view.ts` and `unattended-settings.ts`.

import { html, nothing, svg, type TemplateResult } from 'lit-html';

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
import { systemSettings, updateSettings } from './system-settings';
import type { UnattendedStatus } from './unattended-settings';
import { unattendedSettings } from './unattended-settings';

/** The four sections the panels are grouped into. */
export type SettingsTab = 'system' | 'access' | 'recordings' | 'about';

/**
 * The tabs, in order, with the label key each one carries.
 *
 * A flat list of six panels had the address book, the unattended password,
 * invite revocation, recordings, the audit log and the system switches in one
 * scroll, which is what made finding any of them a hunt. The grouping is by
 * the question being answered: how this window looks and behaves, who this
 * machine lets in and how they prove it, what has been kept about sessions
 * that already happened, and which build this is (ADR 0117).
 *
 * `access` holds what used to be a separate Devices tab: the saved devices and
 * the unattended password are one subject read twice — which devices are
 * trusted, and what a trusted device must still prove — and splitting them
 * only decided which half an operator had to click past first.
 */
const TABS: readonly { readonly id: SettingsTab; readonly label: TranslationKey }[] = [
  { id: 'system', label: 'settings.tab.system' },
  { id: 'access', label: 'settings.tab.access' },
  { id: 'recordings', label: 'settings.tab.recordings' },
  { id: 'about', label: 'settings.tab.about' },
];

/** One drawn glyph per section, in the stroke the rest of the chrome uses. */
const TAB_ICONS: Record<SettingsTab, TemplateResult> = {
  system: tabIcon(svg`<path d="M2.5 4.5h6M11.5 4.5h2M2.5 11.5h2M7.5 11.5h6" /><circle cx="10" cy="4.5" r="1.5" /><circle cx="6" cy="11.5" r="1.5" />`),
  access: tabIcon(svg`<path d="M8 1.75 2.75 3.9v3.6c0 3.1 2.2 5.6 5.25 6.75 3.05-1.15 5.25-3.65 5.25-6.75V3.9Z" /><path d="m5.9 8 1.5 1.5L10.3 6.6" />`),
  recordings: tabIcon(svg`<rect x="1.75" y="3.5" width="9" height="9" rx="2" /><path d="m10.75 7 3.5-2v6l-3.5-2" />`),
  about: tabIcon(svg`<circle cx="8" cy="8" r="6.25" /><path d="M8 7.25v4M8 4.9v.1" />`),
};

function tabIcon(body: ReturnType<typeof svg>): TemplateResult {
  return html`<svg
    viewBox="0 0 16 16"
    width="16"
    height="16"
    aria-hidden="true"
    fill="none"
    stroke="currentColor"
    stroke-width="1.4"
    stroke-linecap="round"
    stroke-linejoin="round"
  >${body}</svg>`;
}

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

/**
 * Opens the settings screen on `section`, remembering what to return focus to.
 * Already open, it only switches section: the sidebar's version line and its
 * update notice both land on About.
 */
export function openSettings(section: SettingsTab = 'system'): void {
  if (open) {
    selectTab(section);
    return;
  }
  trigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  open = true;
  tab = section;
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
 * way a keyboard reaches the others. The strip stands as a column beside the
 * panels and lies as a row above them in a narrow window, so both axes move
 * it; the horizontal one follows the document, so it still reads
 * left-to-right in Arabic's mirrored layout.
 */
function onTabKey(event: KeyboardEvent, id: SettingsTab): void {
  const back = document.documentElement.dir === 'rtl' ? 'ArrowRight' : 'ArrowLeft';
  const forward = back === 'ArrowLeft' ? 'ArrowRight' : 'ArrowLeft';
  const step =
    event.key === forward || event.key === 'ArrowDown'
      ? 1
      : event.key === back || event.key === 'ArrowUp'
        ? -1
        : 0;
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
      // How the app itself looks and behaves on this machine.
      return html`${systemSettings(locale, panels.systemCommands, panels.onLocaleChange)}`;
    case 'access':
      // Who this machine lets in: how a trusted device proves it is itself,
      // which devices are trusted, and the code it hands out to invite them.
      return html`
        ${unattendedSettings(panels.unattended, locale, panels.onRefresh)}
        ${addressBook(panels.savedDevices, locale, panels.onRefresh)}
        ${inviteRefreshPanel(locale)}
      `;
    case 'recordings':
      // What this machine has kept about sessions that already happened.
      return html`
        ${recordingsPanel(panels.recordings, locale, panels.recordingsCommands, panels.onRefresh)}
        ${auditPanel(locale, panels.auditCommands)}
      `;
    case 'about':
      // Which build this is, and whether a newer one exists.
      return html`${updateSettings(locale, panels.systemCommands)}`;
  }
}

/**
 * The settings screen: this device, the address book, invite revocation,
 * unattended access, recordings and the audit log, moved here from the main
 * panel (DECISIONS.md D9) and grouped into the sections of [`TABS`].
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
            <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">
              <path d="M3.5 3.5l9 9M12.5 3.5l-9 9" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" />
            </svg>
          </button>
        </div>
        <div
          class="settings-tabs"
          role="tablist"
          aria-orientation="vertical"
          aria-label=${t(locale, 'settings.tabs.label')}
        >
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
                ${TAB_ICONS[entry.id]}<span>${t(locale, entry.label)}</span>
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
