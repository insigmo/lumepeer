// The recordings panel and the export behind it (§9.2; ADR 0031, ADR 0035).
//
// The export itself is Rust; what these tests pin down is the half that made
// it reachable — that a row exists per recording, that the button asks the
// core by *name* and never by path, that a second press cannot start a second
// export over the first one's output, and that a refusal is shown rather than
// swallowed.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { SUPPORTED_LOCALES, t } from './i18n';
import {
  recordingsPanel,
  resetRecordingsPanel,
  onRecordingsStateChange,
  type ExportResult,
  type RecordingEntry,
  type RecordingsCommands,
} from './recordings';

const entries: RecordingEntry[] = [
  { name: 'session-1700000100-ab12cd.lmrc', bytes: 4 * 1024 * 1024, modified: 1700000100, exported: false },
  { name: 'session-1700000000-ab12cd.lmrc', bytes: 2048, modified: 1700000000, exported: true },
];

const answer: ExportResult = {
  dir: '/home/host/.local/share/lumepeer/recordings/exports',
  video: 'session-1700000100-ab12cd.h264',
  audio: 'session-1700000100-ab12cd.opus',
  video_frames: 900,
  audio_packets: 1500,
  events_skipped: 3,
};

let container: HTMLElement;
let commands: RecordingsCommands;
let exportMock: ReturnType<typeof vi.fn>;

/** Re-renders on every state change, exactly as main.ts wires it. */
function mount(rows: RecordingEntry[] = entries): void {
  const paint = (): void => {
    render(recordingsPanel(rows, 'en', commands, () => {}), container);
  };
  onRecordingsStateChange(paint);
  paint();
}

function rows(): HTMLElement[] {
  return [...container.querySelectorAll<HTMLElement>('[data-testid="recording-row"]')];
}

function exportButton(index = 0): HTMLButtonElement {
  const button = rows()[index]?.querySelector<HTMLButtonElement>('[data-testid="recording-export"]');
  if (!button) {
    throw new Error('no export button in that row');
  }
  return button;
}

beforeEach(() => {
  resetRecordingsPanel();
  exportMock = vi.fn().mockResolvedValue(answer);
  commands = {
    list: vi.fn().mockResolvedValue(entries),
    export: exportMock as unknown as RecordingsCommands['export'],
  };
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  onRecordingsStateChange(() => {});
  container.remove();
});

describe('the recordings panel', () => {
  it('says so plainly when this device has recorded nothing', () => {
    mount([]);

    expect(container.querySelector('[data-testid="recordings-empty"]')?.textContent?.trim()).toBe(
      t('en', 'recordings.empty'),
    );
    expect(rows()).toHaveLength(0);
  });

  it('renders one row per recording, newest first as the core ordered them', () => {
    mount();

    expect(rows()).toHaveLength(2);
    expect(rows()[0]?.textContent).toContain('session-1700000100-ab12cd.lmrc');
    expect(rows()[1]?.textContent).toContain('session-1700000000-ab12cd.lmrc');
  });

  it('offers a re-export for a recording that already has one', () => {
    mount();

    expect(exportButton(0).textContent?.trim()).toBe(t('en', 'recordings.export'));
    expect(exportButton(1).textContent?.trim()).toBe(t('en', 'recordings.exportAgain'));
  });

  it('asks the core by name and never by path', async () => {
    mount();

    exportButton(0).click();
    await vi.waitFor(() => expect(exportMock).toHaveBeenCalledTimes(1));

    expect(exportMock).toHaveBeenCalledWith('session-1700000100-ab12cd.lmrc');
  });

  it('shows the tracks the export produced', async () => {
    mount();

    exportButton(0).click();
    await vi.waitFor(() =>
      expect(container.querySelector('[data-testid="export-done"]')).not.toBeNull(),
    );

    const note = container.querySelector('[data-testid="export-done"]');
    expect(note?.textContent).toContain('session-1700000100-ab12cd.h264');
    expect(note?.textContent).toContain('session-1700000100-ab12cd.opus');
    // The directory is the core's answer, shown and not chosen here.
    expect(note?.getAttribute('title')).toBe(answer.dir);
  });

  it('does not claim a file when the recording held no track', async () => {
    exportMock.mockResolvedValue({
      ...answer,
      video: null,
      audio: null,
      video_frames: 0,
      audio_packets: 0,
    });
    mount();

    exportButton(0).click();
    await vi.waitFor(() =>
      expect(container.querySelector('[data-testid="export-empty"]')).not.toBeNull(),
    );
    expect(container.querySelector('[data-testid="export-done"]')).toBeNull();
  });

  it('refuses a second press while the first export is still running', async () => {
    let finish: (value: ExportResult) => void = () => {};
    exportMock.mockImplementation(
      () =>
        new Promise<ExportResult>((resolve) => {
          finish = resolve;
        }),
    );
    mount();

    exportButton(0).click();
    await vi.waitFor(() => expect(exportButton(0).disabled).toBe(true));
    expect(exportButton(0).textContent?.trim()).toBe(t('en', 'recordings.exporting'));
    exportButton(0).click();

    finish(answer);
    await vi.waitFor(() => expect(exportButton(0).disabled).toBe(false));
    expect(exportMock).toHaveBeenCalledTimes(1);
  });

  it('shows a refusal instead of swallowing it', async () => {
    const error = new Error('bad recording');
    exportMock.mockRejectedValue(Object.assign(error, { code: 'BAD_RECORDING' }));
    const logged = vi.spyOn(console, 'error').mockImplementation(() => {});
    mount();

    exportButton(0).click();
    await vi.waitFor(() =>
      expect(container.querySelector('[data-testid="export-failed"]')).not.toBeNull(),
    );

    expect(container.querySelector('[data-testid="export-failed"]')?.textContent?.trim()).toBe(
      t('en', 'recordings.exportFailed'),
    );
    // The button comes back rather than staying stuck on "exporting".
    expect(exportButton(0).disabled).toBe(false);
    logged.mockRestore();
  });

  it('is translated in every supported locale', () => {
    for (const locale of SUPPORTED_LOCALES) {
      render(recordingsPanel(entries, locale, commands, () => {}), container);
      for (const key of [
        'recordings.heading',
        'recordings.export',
        'recordings.exportAgain',
        'recordings.exporting',
        'recordings.exportedNothing',
        'recordings.exportFailed',
      ] as const) {
        expect(t(locale, key)).not.toBe('');
      }
      expect(container.textContent).toContain(t(locale, 'recordings.heading'));
    }
  });
});
