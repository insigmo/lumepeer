// The two-pane file manager (design doc §9.2, §18; ADR 0075, ADR 0076).
//
// The tests are about the four things the panel is responsible for and the
// Rust side is not: that a refusal reads as the refusal it is rather than as
// "nothing found"; that a truncated listing says so instead of passing for a
// short directory; that download and upload name exactly the paths the two
// panes are showing; and that a panel the host has not granted disappears
// rather than sitting there as a button that quietly does nothing.
import * as axe from 'axe-core';
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  browseDenied,
  emptyState,
  fileManagerPanel,
  joinPath,
  refresh,
  REMOTE_ROOTS,
  type FileManagerCommands,
  type FileManagerState,
  type LocalDir,
  type RemoteDir,
} from './file-manager';
import type { FileCommands, FileTransfers } from './file-transfers';
import { SUPPORTED_LOCALES, t } from './i18n';

const PEER = 'host-ab12';

const NO_TRANSFERS: FileTransfers = { offers: [], transfers: [] };

function fileCommands(): FileCommands {
  return {
    accept: vi.fn().mockResolvedValue(undefined),
    abort: vi.fn().mockResolvedValue(undefined),
    list: vi.fn().mockResolvedValue(NO_TRANSFERS),
  };
}

function localDir(over: Partial<LocalDir> = {}): LocalDir {
  return {
    path: '/home/beta',
    parent: '/home',
    entries: [
      { name: 'notes.txt', size: 2048, is_dir: false, modified_unix: 0 },
      { name: 'projects', size: 0, is_dir: true, modified_unix: 0 },
    ],
    truncated: false,
    ...over,
  };
}

function remoteDir(over: Partial<RemoteDir> = {}): RemoteDir {
  return {
    path: '/srv/share',
    parent: '/srv',
    entries: [{ name: 'report.pdf', size: 4096, is_dir: false, modified_unix: 0 }],
    truncated: false,
    refused: null,
    fetch_refused: null,
    answered: true,
    ...over,
  };
}

function commands(over: Partial<FileManagerCommands> = {}): FileManagerCommands {
  return {
    localList: vi.fn().mockResolvedValue(localDir()),
    remoteList: vi.fn().mockResolvedValue(undefined),
    remoteStatus: vi.fn().mockResolvedValue(remoteDir()),
    download: vi.fn().mockResolvedValue(undefined),
    upload: vi.fn().mockResolvedValue(undefined),
    ...over,
  };
}

function state(over: Partial<FileManagerState> = {}): FileManagerState {
  return { ...emptyState(), local: localDir(), remote: remoteDir(), ...over };
}

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

function draw(
  panelState: FileManagerState,
  cmds: FileManagerCommands,
  transfers: FileTransfers = NO_TRANSFERS,
): void {
  render(
    fileManagerPanel(PEER, panelState, transfers, 'en', cmds, fileCommands()),
    container,
  );
}

function press(testid: string): void {
  container.querySelector<HTMLButtonElement>(`[data-testid="${testid}"]`)?.click();
}

describe('the two panes', () => {
  it('show a path, a list and an up button on each side', () => {
    draw(state(), commands());
    expect(container.querySelector('[data-testid="fm-local-path"]')?.textContent).toBe(
      '/home/beta',
    );
    expect(container.querySelector('[data-testid="fm-remote-path"]')?.textContent).toBe(
      '/srv/share',
    );
    expect(container.querySelectorAll('[data-testid="fm-local-entry"]')).toHaveLength(2);
    expect(container.querySelectorAll('[data-testid="fm-remote-entry"]')).toHaveLength(1);
    expect(
      container.querySelector<HTMLButtonElement>('[data-testid="fm-local-up"]')?.disabled,
    ).toBe(false);
  });

  it('cannot go up from a root, because there is nowhere above it', () => {
    draw(state({ local: localDir({ path: '/', parent: null }) }), commands());
    expect(
      container.querySelector<HTMLButtonElement>('[data-testid="fm-local-up"]')?.disabled,
    ).toBe(true);
  });

  it('opens a directory by name on the side it was pressed', () => {
    const cmds = commands();
    draw(state(), cmds);
    // The second local entry is the directory.
    container
      .querySelectorAll<HTMLButtonElement>('[data-testid="fm-local-entry"]')[1]
      ?.click();
    expect(cmds.localList).toHaveBeenCalledWith('/home/beta/projects');
  });
});

describe('an empty, a refused and a truncated listing', () => {
  it('are three different sentences and not one "nothing found"', () => {
    draw(state({ remote: remoteDir({ entries: [] }) }), commands());
    expect(container.querySelector('[data-testid="fm-remote-empty"]')?.textContent?.trim()).toBe(
      t('en', 'fileManager.empty'),
    );

    draw(state({ remote: remoteDir({ entries: [], refused: 'unreadable' }) }), commands());
    expect(
      container.querySelector('[data-testid="fm-remote-refused"]')?.textContent?.trim(),
    ).toBe(t('en', 'fileManager.refused.unreadable'));
    expect(container.querySelector('[data-testid="fm-remote-empty"]')).toBeNull();

    draw(state({ remote: remoteDir({ truncated: true }) }), commands());
    expect(
      container.querySelector('[data-testid="fm-remote-truncated"]')?.textContent?.trim(),
    ).toBe(t('en', 'fileManager.truncated'));
  });

  it('says why a download was refused rather than dropping it silently', () => {
    draw(state({ remote: remoteDir({ fetch_refused: 'too_many' }) }), commands());
    expect(
      container.querySelector('[data-testid="fm-download-refused"]')?.textContent?.trim(),
    ).toBe(t('en', 'fileManager.download.tooMany'));
  });
});

describe('a panel the host has not granted', () => {
  it('is not drawn at all, rather than drawn and inert', () => {
    const denied = state({ remote: remoteDir({ entries: [], refused: 'not_granted' }) });
    expect(browseDenied(denied)).toBe(true);
    draw(denied, commands());
    expect(container.querySelector('[data-testid="file-manager"]')).toBeNull();
  });

  it('is drawn again the moment the host allows it', () => {
    const allowed = state();
    expect(browseDenied(allowed)).toBe(false);
    draw(allowed, commands());
    expect(container.querySelector('[data-testid="file-manager"]')).not.toBeNull();
  });
});

describe('download and upload', () => {
  it('name the selected entry and the other pane’s directory', () => {
    const cmds = commands();
    const panelState = state({ remoteSelected: 'report.pdf', localSelected: 'notes.txt' });
    draw(panelState, cmds);

    press('fm-download');
    expect(cmds.download).toHaveBeenCalledWith('/srv/share/report.pdf', '/home/beta');

    press('fm-upload');
    expect(cmds.upload).toHaveBeenCalledWith('/home/beta/notes.txt', '/srv/share');
  });

  it('are both refused by the panel until something is selected', () => {
    const cmds = commands();
    draw(state(), cmds);
    press('fm-download');
    press('fm-upload');
    expect(cmds.download).not.toHaveBeenCalled();
    expect(cmds.upload).not.toHaveBeenCalled();
  });

  it('join a Windows path with the separator that host uses', () => {
    expect(joinPath('C:\\Users\\beta', 'notes.txt')).toBe('C:\\Users\\beta\\notes.txt');
    expect(joinPath('C:\\', 'notes.txt')).toBe('C:\\notes.txt');
    expect(joinPath('/home/beta', 'notes.txt')).toBe('/home/beta/notes.txt');
    expect(joinPath('/', 'notes.txt')).toBe('/notes.txt');
  });
});

describe('the first listing', () => {
  it('probes the roots in turn, because no message asks a host where it keeps its files', async () => {
    const panelState = emptyState();
    const cmds = commands({
      remoteStatus: vi.fn().mockResolvedValue(remoteDir({ answered: false, entries: [] })),
    });
    await refresh(panelState, cmds);
    expect(cmds.remoteList).toHaveBeenCalledWith(REMOTE_ROOTS[0]);

    const refusing = commands({
      remoteStatus: vi
        .fn()
        .mockResolvedValue(remoteDir({ answered: true, entries: [], refused: 'bad_path' })),
    });
    await refresh(panelState, refusing);
    expect(refusing.remoteList).toHaveBeenCalledWith(REMOTE_ROOTS[1]);
  });

  it('stops probing once the host has refused for want of a grant', async () => {
    const panelState = emptyState();
    const cmds = commands({
      remoteStatus: vi
        .fn()
        .mockResolvedValue(remoteDir({ answered: true, entries: [], refused: 'not_granted' })),
    });
    await refresh(panelState, cmds);
    expect(cmds.remoteList).not.toHaveBeenCalled();
  });
});

describe('accessibility', () => {
  const LAYOUT_DEPENDENT_RULES = ['color-contrast', 'target-size'];

  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations, and every control is a real button (${locale})`, async () => {
      render(
        fileManagerPanel(
          PEER,
          state({ remote: remoteDir({ truncated: true, fetch_refused: 'unreadable' }) }),
          NO_TRANSFERS,
          locale,
          commands(),
          fileCommands(),
        ),
        container,
      );
      const results = await axe.run(container, {
        rules: Object.fromEntries(LAYOUT_DEPENDENT_RULES.map((id) => [id, { enabled: false }])),
      });
      expect(results.violations).toEqual([]);
      // Every interactive element is a `<button>`, so Tab reaches all of them
      // without a tabindex anywhere.
      const interactive = container.querySelectorAll('[data-testid^="fm-"]');
      for (const element of interactive) {
        if (element.tagName === 'BUTTON' || element.tagName === 'P' || element.tagName === 'UL') {
          continue;
        }
        throw new Error(`unexpected element in the file manager: ${element.tagName}`);
      }
      expect(container.querySelectorAll('[tabindex]')).toHaveLength(0);
    });
  }
});
