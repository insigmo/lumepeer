// Host-side audit log panel (§15; ADR 0041).
//
// `crates/core/src/audit.rs` has always defined the events and the
// pseudonymized record; `audit_store.rs` now writes them. This panel is the
// half §15 asks for on top of the storage: the host user must be able to read
// the log, take it away and erase it, and none of that is reachable from a
// database file nobody is told about.
//
// Nothing here names a path. `audit_export` opens the OS save dialog in Rust
// and returns where it wrote; the webview holds no `fs` permission (§2.3).
// Nothing here filters for privacy either — the rows arrive pseudonymized,
// because the pseudonymization happens before the row is stored.

import { html, type TemplateResult } from 'lit-html';

import type { Locale, TranslationKey } from './i18n';
import { t } from './i18n';

/** One row of `audit_list`. */
export interface AuditRow {
  /** Wall-clock second the event was recorded at. */
  at_unix_secs: number;
  /** Pseudonymized peer label: a hash prefix, never an identity. */
  peer: string;
  /** Event kind from the closed vocabulary `audit_kinds` serves. */
  kind: string;
  /** Extra detail from the same closed vocabulary, or empty. */
  detail: string;
}

/** How this panel reaches the actor; injectable so tests need no Tauri. */
export interface AuditCommands {
  status(): Promise<boolean>;
  kinds(): Promise<string[]>;
  list(since: number | null, until: number | null, kind: string | null): Promise<AuditRow[]>;
  export(): Promise<string | null>;
  clear(): Promise<number>;
}

export const tauriAuditCommands: AuditCommands = {
  async status() {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('audit_status')) as boolean;
  },
  async kinds() {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('audit_kinds')) as string[];
  },
  async list(since, until, kind) {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('audit_list', { args: { since, until, kind } })) as AuditRow[];
  },
  async export() {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('audit_export')) as string | null;
  },
  async clear() {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('audit_clear')) as number;
  },
};

/** Panel state. Local on purpose: nothing else in the app reads the log. */
interface State {
  loaded: boolean;
  enabled: boolean;
  rows: AuditRow[];
  kinds: string[];
  /** Selected kind, or '' for every kind. */
  kind: string;
  /** `yyyy-mm-dd` bounds as the date inputs give them, or '' for unbounded. */
  from: string;
  to: string;
  busy: boolean;
  confirmingClear: boolean;
  notice: TranslationKey | null;
  noticeArg: string;
}

const state: State = {
  loaded: false,
  enabled: false,
  rows: [],
  kinds: [],
  kind: '',
  from: '',
  to: '',
  busy: false,
  confirmingClear: false,
  notice: null,
  noticeArg: '',
};

let onChange: (() => void) | undefined;

/** Lets main.ts re-render after an async change here. */
export function onAuditStateChange(callback: () => void): void {
  onChange = callback;
}

/** Test seam: drops the panel's state between cases. */
export function resetAuditPanel(): void {
  state.loaded = false;
  state.enabled = false;
  state.rows = [];
  state.kinds = [];
  state.kind = '';
  state.from = '';
  state.to = '';
  state.busy = false;
  state.confirmingClear = false;
  state.notice = null;
  state.noticeArg = '';
}

/** Start of a `yyyy-mm-dd` day in Unix seconds, or null when unset. */
function dayStart(value: string): number | null {
  if (value === '') {
    return null;
  }
  const parsed = Date.parse(`${value}T00:00:00`);
  return Number.isNaN(parsed) ? null : Math.floor(parsed / 1000);
}

/** End of a `yyyy-mm-dd` day in Unix seconds, so "to" includes that day. */
function dayEnd(value: string): number | null {
  const start = dayStart(value);
  return start === null ? null : start + 24 * 60 * 60 - 1;
}

async function reload(commands: AuditCommands): Promise<void> {
  try {
    state.enabled = await commands.status();
    if (!state.loaded) {
      state.kinds = await commands.kinds();
    }
    state.rows = await commands.list(
      dayStart(state.from),
      dayEnd(state.to),
      state.kind === '' ? null : state.kind,
    );
    state.loaded = true;
  } catch (error) {
    console.error('audit_list failed:', error);
    state.notice = 'audit.loadFailed';
    state.noticeArg = '';
    state.loaded = true;
  }
  onChange?.();
}

/** A stored `kind` as a sentence, falling back to the raw tag. */
function kindLabel(kind: string, locale: Locale): string {
  const key = `audit.kind.${kind}` as TranslationKey;
  const label = t(locale, key);
  return label === '' ? kind : label;
}

/**
 * The audit log with a date and kind filter, an export and a purge (§15).
 *
 * Fetches on first render rather than on the caller's poll: a log is read when
 * someone looks at it, and re-querying it every two seconds next to the
 * session list would be a table scan nobody asked for.
 */
export function auditPanel(
  locale: Locale,
  commands: AuditCommands = tauriAuditCommands,
): TemplateResult {
  if (!state.loaded && !state.busy) {
    state.busy = true;
    void reload(commands).finally(() => {
      state.busy = false;
      onChange?.();
    });
  }

  const apply = (): void => {
    if (state.busy) {
      return;
    }
    state.busy = true;
    state.notice = null;
    onChange?.();
    void reload(commands).finally(() => {
      state.busy = false;
      onChange?.();
    });
  };

  return html`
    <section class="audit" data-testid="audit-panel">
      <h3>${t(locale, 'audit.heading')}</h3>
      ${state.loaded && !state.enabled
        ? html`<p class="audit-disabled" role="status" data-testid="audit-disabled">
            ${t(locale, 'audit.disabled')}
          </p>`
        : ''}
      <div class="audit-filters">
        <label class="audit-filter">
          <span>${t(locale, 'audit.filterFrom')}</span>
          <input
            type="date"
            data-testid="audit-from"
            .value=${state.from}
            @change=${(event: Event) => {
              state.from = (event.target as HTMLInputElement).value;
            }}
          />
        </label>
        <label class="audit-filter">
          <span>${t(locale, 'audit.filterTo')}</span>
          <input
            type="date"
            data-testid="audit-to"
            .value=${state.to}
            @change=${(event: Event) => {
              state.to = (event.target as HTMLInputElement).value;
            }}
          />
        </label>
        <label class="audit-filter">
          <span>${t(locale, 'audit.filterKind')}</span>
          <select
            data-testid="audit-kind"
            .value=${state.kind}
            @change=${(event: Event) => {
              state.kind = (event.target as HTMLSelectElement).value;
            }}
          >
            <option value="">${t(locale, 'audit.filterAll')}</option>
            ${state.kinds.map(
              (kind) => html`<option value=${kind}>${kindLabel(kind, locale)}</option>`,
            )}
          </select>
        </label>
        <button type="button" data-testid="audit-apply" ?disabled=${state.busy} @click=${apply}>
          ${t(locale, 'audit.apply')}
        </button>
      </div>
      ${state.loaded && state.enabled && state.rows.length === 0
        ? html`<p class="audit-empty" data-testid="audit-empty">${t(locale, 'audit.empty')}</p>`
        : ''}
      ${state.rows.length === 0
        ? ''
        : html`
            <table class="audit-table" data-testid="audit-table">
              <thead>
                <tr>
                  <th scope="col">${t(locale, 'audit.time')}</th>
                  <th scope="col">${t(locale, 'audit.peer')}</th>
                  <th scope="col">${t(locale, 'audit.event')}</th>
                  <th scope="col">${t(locale, 'audit.detail')}</th>
                </tr>
              </thead>
              <tbody>
                ${state.rows.map(
                  (row) => html`
                    <tr data-testid="audit-row">
                      <td>${new Date(row.at_unix_secs * 1000).toLocaleString(locale)}</td>
                      <td class="audit-peer">${row.peer}</td>
                      <td>${kindLabel(row.kind, locale)}</td>
                      <td>${row.detail}</td>
                    </tr>
                  `,
                )}
              </tbody>
            </table>
          `}
      <div class="audit-actions">
        <button
          type="button"
          data-testid="audit-export"
          ?disabled=${state.busy || !state.enabled}
          @click=${() => {
            if (state.busy) {
              return;
            }
            state.busy = true;
            state.notice = null;
            onChange?.();
            void commands.export().then(
              (path) => {
                state.busy = false;
                if (path !== null) {
                  state.notice = 'audit.exported';
                  state.noticeArg = path;
                }
                onChange?.();
              },
              (error: unknown) => {
                state.busy = false;
                console.error('audit_export failed:', error);
                state.notice = 'audit.exportFailed';
                state.noticeArg = '';
                onChange?.();
              },
            );
          }}
        >
          ${t(locale, 'audit.export')}
        </button>
        ${state.confirmingClear
          ? html`
              <span class="audit-confirm" role="status" data-testid="audit-confirm">
                ${t(locale, 'audit.clearConfirm')}
              </span>
              <button
                type="button"
                class="audit-clear-yes"
                data-testid="audit-clear-confirm"
                ?disabled=${state.busy}
                @click=${() => {
                  state.confirmingClear = false;
                  state.busy = true;
                  state.notice = null;
                  onChange?.();
                  void commands.clear().then(
                    (removed) => {
                      state.busy = false;
                      state.notice = 'audit.cleared';
                      state.noticeArg = String(removed);
                      state.rows = [];
                      onChange?.();
                    },
                    (error: unknown) => {
                      state.busy = false;
                      console.error('audit_clear failed:', error);
                      state.notice = 'audit.clearFailed';
                      state.noticeArg = '';
                      onChange?.();
                    },
                  );
                }}
              >
                ${t(locale, 'audit.clearYes')}
              </button>
              <button
                type="button"
                data-testid="audit-clear-cancel"
                @click=${() => {
                  state.confirmingClear = false;
                  onChange?.();
                }}
              >
                ${t(locale, 'audit.clearNo')}
              </button>
            `
          : html`
              <button
                type="button"
                data-testid="audit-clear"
                ?disabled=${state.busy || !state.enabled}
                @click=${() => {
                  state.confirmingClear = true;
                  onChange?.();
                }}
              >
                ${t(locale, 'audit.clear')}
              </button>
            `}
        ${state.notice === null
          ? ''
          : html`<span class="audit-notice" role="status" data-testid="audit-notice"
              >${t(locale, state.notice, state.noticeArg)}</span
            >`}
      </div>
    </section>
  `;
}
