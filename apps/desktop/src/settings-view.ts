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
import type { Locale } from './i18n';
import { t } from './i18n';
import type { RecordingEntry, RecordingsCommands } from './recordings';
import { recordingsPanel } from './recordings';
import type { SystemCommands } from './system-settings';
import { systemSettings } from './system-settings';
import type { UnattendedStatus } from './unattended-settings';
import { unattendedSettings } from './unattended-settings';

let open = false;
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

/** Test seam: drops transient state (open, remembered focus) between cases. */
export function resetSettingsView(): void {
  open = false;
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
}

/**
 * The settings screen: address book, unattended access, recordings, audit
 * log, this device, and invite revocation, moved here from the main panel
 * (DECISIONS.md D9). None of these panels are rewritten — each keeps its
 * own render function and arguments; this module only places them.
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
        <div class="settings-body">
          ${addressBook(panels.savedDevices, locale, panels.onRefresh)}
          ${unattendedSettings(panels.unattended, locale, panels.onRefresh)}
          ${inviteRefreshPanel(locale)}
          ${recordingsPanel(panels.recordings, locale, panels.recordingsCommands, panels.onRefresh)}
          ${auditPanel(locale, panels.auditCommands)}
          ${systemSettings(locale, panels.systemCommands)}
        </div>
      </section>
    </div>
  `;
}
