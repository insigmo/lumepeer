// Host-side recordings and their export (§9.2, §17; ADR 0031, ADR 0035).
//
// The recorder has always written `.lmrc` files and the media crate has
// always been able to turn one into an H.264 elementary stream plus an Ogg
// Opus stream. This panel is the only way a person reaches either: without
// it the export is code nobody can run, and the recordings are files nobody
// is told about.
//
// Nothing here names a path. `recordings_list` hands out file names, the
// export takes one back, and the directory is joined in Rust — the untrusted
// view layer says *which of this device's recordings*, never *which file on
// this disk* (§2.3).

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';

/** One row of `recordings_list`. */
export interface RecordingEntry {
  /** File name inside the app's recordings directory, never a path. */
  name: string;
  bytes: number;
  /** Unix seconds of the last write. */
  modified: number;
  /** Whether an export of this recording already exists. */
  exported: boolean;
}

/** What `recording_export` answered. */
export interface ExportResult {
  /** Directory the tracks were written into, chosen in Rust and shown here. */
  dir: string;
  video: string | null;
  audio: string | null;
  video_frames: number;
  audio_packets: number;
  events_skipped: number;
}

/** How this panel reaches the actor; injectable so tests need no Tauri. */
export interface RecordingsCommands {
  list(): Promise<RecordingEntry[]>;
  export(name: string): Promise<ExportResult>;
}

export const tauriRecordingsCommands: RecordingsCommands = {
  async list() {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('recordings_list')) as RecordingEntry[];
  },
  async export(name: string) {
    const { invoke } = await import('@tauri-apps/api/core');
    return (await invoke('recording_export', { args: { name } })) as ExportResult;
  },
};

/** Exports in flight, by recording name. */
const running = new Set<string>();
/** The last answer per recording, so a finished export says where it went. */
const results = new Map<string, ExportResult>();
/** The last failure per recording, cleared when it is retried. */
const failures = new Map<string, string>();
let onChange: (() => void) | undefined;

/** Lets main.ts re-render after an async change here. */
export function onRecordingsStateChange(callback: () => void): void {
  onChange = callback;
}

/** Test seam: drops the transient state between cases. */
export function resetRecordingsPanel(): void {
  running.clear();
  results.clear();
  failures.clear();
}

const KIB = 1024;
const MIB = KIB * KIB;

/**
 * Size of a recording, coarse on purpose.
 *
 * An exact byte count says nothing a host wants to know; "is this the
 * two-minute one or the hour-long one" does.
 */
function formatSize(bytes: number, locale: Locale): string {
  if (bytes >= MIB) {
    return t(locale, 'recordings.megabytes', String(Math.max(1, Math.round(bytes / MIB))));
  }
  return t(locale, 'recordings.kilobytes', String(Math.max(1, Math.round(bytes / KIB))));
}

/** The tracks one export produced, as one line. */
function exportSummary(result: ExportResult, locale: Locale): TemplateResult {
  const tracks = [result.video, result.audio].filter((name): name is string => name !== null);
  if (tracks.length === 0) {
    return html`<span class="recording-export-note" data-testid="export-empty"
      >${t(locale, 'recordings.exportedNothing')}</span
    >`;
  }
  return html`<span class="recording-export-note" data-testid="export-done" title=${result.dir}
    >${t(locale, 'recordings.exportedTo', tracks.join(', '))}</span
  >`;
}

/**
 * Everything this device has recorded, and one button per row that turns a
 * recording into files a player opens (§9.2).
 *
 * `entries` comes from the caller's poll rather than from a fetch here, so the
 * list is refreshed on the same beat as the session list and a stopped
 * recording shows up without anyone pressing anything.
 */
export function recordingsPanel(
  entries: RecordingEntry[],
  locale: Locale,
  commands: RecordingsCommands = tauriRecordingsCommands,
  onRefresh: () => void = () => {},
): TemplateResult {
  return html`
    <section class="recordings" data-testid="recordings-panel">
      <h3>${t(locale, 'recordings.heading')}</h3>
      ${entries.length === 0
        ? html`<p class="recordings-empty" data-testid="recordings-empty">
            ${t(locale, 'recordings.empty')}
          </p>`
        : html`
            <ul class="recordings-list">
              ${entries.map((entry) => {
                const busy = running.has(entry.name);
                const result = results.get(entry.name);
                const failure = failures.get(entry.name);
                return html`
                  <li data-testid="recording-row">
                    <span class="recording-name" title=${entry.name}>${entry.name}</span>
                    <span class="recording-size">${formatSize(entry.bytes, locale)}</span>
                    <button
                      type="button"
                      class="recording-export-btn"
                      data-testid="recording-export"
                      ?disabled=${busy}
                      aria-label=${`${t(locale, 'recordings.export')}: ${entry.name}`}
                      @click=${() => {
                        // Guard the double press: the second click would run a
                        // second streaming read of the same file over the
                        // first one's output.
                        if (running.has(entry.name)) {
                          return;
                        }
                        running.add(entry.name);
                        failures.delete(entry.name);
                        onChange?.();
                        void commands.export(entry.name).then(
                          (answer) => {
                            running.delete(entry.name);
                            results.set(entry.name, answer);
                            // Re-poll: the row's `exported` flag is the Rust
                            // side's answer, not this one's guess.
                            onRefresh();
                            onChange?.();
                          },
                          (error: unknown) => {
                            running.delete(entry.name);
                            console.error('recording_export failed:', error);
                            failures.set(entry.name, String((error as { code?: string })?.code ?? ''));
                            onChange?.();
                          },
                        );
                      }}
                    >
                      ${busy
                        ? t(locale, 'recordings.exporting')
                        : t(locale, entry.exported ? 'recordings.exportAgain' : 'recordings.export')}
                    </button>
                    ${result ? exportSummary(result, locale) : ''}
                    ${failure === undefined
                      ? ''
                      : html`<span class="recording-export-error" role="status" data-testid="export-failed"
                          >${t(locale, 'recordings.exportFailed')}</span
                        >`}
                  </li>
                `;
              })}
            </ul>
          `}
    </section>
  `;
}
