// The file manager window (design doc §9.2, §18; ADR 0075, ADR 0076,
// ADR 0124).
//
// The tests are about what the window is responsible for and the Rust side is
// not: that the first listing finds the host's disk without a message that
// asks where it is; that a refusal reads as the refusal it is rather than as
// "nothing found"; that a transfer names exactly the paths the two panes are
// showing, whether it starts from a button or a drag; that a change the host
// or this machine refused says why; and that nothing is deleted without a
// second "yes".
import * as axe from 'axe-core';
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  FileManager,
  joinPath,
  REMOTE_ROOTS,
  renderFileManager,
  type FileManagerCommands,
  type FileOp,
  type LocalDir,
  type RemoteDir,
} from './file-manager';
import type { FileCommands, FileTransfers } from './file-transfers';
import { SUPPORTED_LOCALES, t, type Locale } from './i18n';

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
    entries: [
      { name: 'report.pdf', size: 4096, is_dir: false, modified_unix: 0 },
      { name: 'inbox', size: 0, is_dir: true, modified_unix: 0 },
    ],
    truncated: false,
    refused: null,
    fetch_refused: null,
    answered: true,
    fetches_refused: 0,
    uploads_refused: 0,
    op_results: [],
    listings_answered: 0,
    ...over,
  };
}

/**
 * A host that answers every listing request at once, from `answer`.
 *
 * The real answer arrives on a later `remote_dir_status` with the listing
 * counter moved on; this does the same, one request per counter step.
 */
function fakeHost(answer: (path: string) => Partial<RemoteDir>) {
  const status = remoteDir({ path: '', parent: null, entries: [], answered: false });
  const listed: string[] = [];
  const commands: FileManagerCommands = {
    localList: vi.fn().mockResolvedValue(localDir()),
    localRoots: vi.fn().mockResolvedValue(['/home/beta', '/']),
    localOp: vi.fn<(op: FileOp) => Promise<void>>().mockResolvedValue(undefined),
    remoteList: vi.fn(async (path: string) => {
      listed.push(path);
      Object.assign(
        status,
        remoteDir({ ...answer(path), listings_answered: status.listings_answered + 1 }),
        {
          fetches_refused: status.fetches_refused,
          uploads_refused: status.uploads_refused,
          op_results: status.op_results,
        },
      );
    }),
    remoteStatus: vi.fn(async () => ({ ...status })),
    remoteOp: vi.fn<(op: FileOp) => Promise<number>>().mockResolvedValue(7),
    download: vi.fn().mockResolvedValue(undefined),
    upload: vi.fn().mockResolvedValue(undefined),
  };
  return { status, listed, commands };
}

/** A host whose disk is `/srv/share`, as a Unix host's is. */
function unixHost() {
  return fakeHost((path) =>
    path === 'C:\\'
      ? { path: '', parent: null, entries: [], refused: 'bad_path' }
      : { path: '/srv/share' },
  );
}

/** Lets every promise the controller left running settle. */
async function settle(): Promise<void> {
  for (let i = 0; i < 10; i += 1) {
    await Promise.resolve();
  }
}

function manager(commands: FileManagerCommands, files: FileCommands = fileCommands()) {
  return new FileManager({
    peer: PEER,
    locale: 'en',
    commands,
    fileCommands: files,
    onChange: () => undefined,
  });
}

/** A file manager with both panes showing a folder, as a person first sees it. */
async function opened(host = unixHost(), files: FileCommands = fileCommands()) {
  const fm = manager(host.commands, files);
  await fm.start();
  await fm.poll();
  await settle();
  await fm.poll();
  await settle();
  return fm;
}

/** A press on a row, as the pointer delivers it. */
function press(x = 0, y = 0): PointerEvent {
  return {
    button: 0,
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
    clientX: x,
    clientY: y,
  } as PointerEvent;
}

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

function draw(fm: FileManager, locale: Locale = 'en', standalone = false): void {
  render(renderFileManager(fm, locale, { standalone, onDisconnect: () => undefined }), container);
}

function text(testid: string): string | undefined {
  return container.querySelector(`[data-testid="${testid}"]`)?.textContent?.trim();
}

describe('the first listing', () => {
  it('probes the roots in turn, because no message asks a host where it keeps its files', async () => {
    const host = unixHost();
    const fm = await opened(host);
    expect(host.listed).toEqual([REMOTE_ROOTS[0], REMOTE_ROOTS[1]]);
    expect(fm.remote.path).toBe('/srv/share');
    expect(fm.local.path).toBe('/home/beta');
    expect(fm.remote.problem).toBeNull();
  });

  it('stops probing once the host has refused for want of a grant, and says so', async () => {
    const host = fakeHost(() => ({ path: '', parent: null, entries: [], refused: 'not_granted' }));
    const fm = await opened(host);
    expect(host.listed).toEqual([REMOTE_ROOTS[0]]);
    draw(fm);
    expect(text('fm-remote-problem')).toBe(t('en', 'fileManager.refused.notGranted'));
    expect(container.querySelector('[data-testid="fm-remote-list"]')).toBeNull();
  });

  it('says the host is too old rather than waiting on it forever', async () => {
    const host = unixHost();
    host.commands.remoteList = vi.fn().mockRejectedValue({ code: 'PEER_TOO_OLD', message: '' });
    const fm = await opened(host);
    draw(fm);
    expect(text('fm-remote-problem')).toBe(t('en', 'fm.error.tooOld'));
  });
});

describe('an empty and a truncated listing', () => {
  it('are different sentences, and neither passes for the other', async () => {
    const empty = await opened(fakeHost((path) => ({ path, entries: [] })));
    draw(empty);
    expect(text('fm-remote-empty')).toBe(t('en', 'fileManager.empty'));
    expect(container.querySelector('[data-testid="fm-remote-truncated"]')).toBeNull();

    const truncated = await opened(fakeHost(() => ({ path: '/srv/share', truncated: true })));
    draw(truncated);
    expect(text('fm-remote-truncated')).toBe(t('en', 'fileManager.truncated'));
    expect(container.querySelector('[data-testid="fm-remote-empty"]')).toBeNull();
  });
});

describe('moving files', () => {
  it('uploads the selection into the folder the remote pane is showing', async () => {
    const host = unixHost();
    const fm = await opened(host);
    fm.select('local', 'notes.txt', { ctrl: false, shift: false });
    fm.transferSelection('local');
    await settle();
    expect(host.commands.upload).toHaveBeenCalledWith('/home/beta/notes.txt', '/srv/share');
  });

  it('downloads the selection into the folder the local pane is showing', async () => {
    const host = unixHost();
    const fm = await opened(host);
    fm.select('remote', 'report.pdf', { ctrl: false, shift: false });
    fm.transferSelection('remote');
    await settle();
    expect(host.commands.download).toHaveBeenCalledWith('/srv/share/report.pdf', '/home/beta');
  });

  it('keeps the Upload and Download buttons off until something is selected', async () => {
    const fm = await opened();
    draw(fm);
    expect(container.querySelector<HTMLButtonElement>('[data-testid="fm-upload"]')?.disabled).toBe(
      true,
    );
    expect(
      container.querySelector<HTMLButtonElement>('[data-testid="fm-download"]')?.disabled,
    ).toBe(true);
    fm.select('local', 'notes.txt', { ctrl: false, shift: false });
    draw(fm);
    expect(container.querySelector<HTMLButtonElement>('[data-testid="fm-upload"]')?.disabled).toBe(
      false,
    );
  });

  it('drops a dragged row into the remote folder it was let go over', async () => {
    const host = unixHost();
    const fm = await opened(host);
    fm.pressRow('local', 'notes.txt', press(10, 10));
    fm.dragMove(60, 10, { side: 'remote', folder: 'inbox' });
    expect(fm.dragEnd()).toBe(true);
    await settle();
    expect(host.commands.upload).toHaveBeenCalledWith('/home/beta/notes.txt', '/srv/share/inbox');
  });

  it('does not treat a click as a drag, nor a drop back on its own pane as a copy', async () => {
    const host = unixHost();
    const fm = await opened(host);
    fm.pressRow('local', 'notes.txt', press(10, 10));
    fm.dragMove(12, 11, { side: 'remote', folder: null });
    expect(fm.dragEnd()).toBe(false);

    fm.pressRow('local', 'notes.txt', press(10, 10));
    fm.dragMove(60, 60, { side: 'local', folder: 'projects' });
    fm.dragEnd();
    await settle();
    expect(host.commands.upload).not.toHaveBeenCalled();
  });

  it('pastes on the other side only, and says so on the same side', async () => {
    const host = unixHost();
    const fm = await opened(host);
    fm.select('remote', 'report.pdf', { ctrl: false, shift: false });
    fm.copy('remote');
    fm.paste('remote');
    expect(fm.notice?.text).toBe(t('en', 'fm.pasteOther'));
    expect(host.commands.download).not.toHaveBeenCalled();
    fm.paste('local');
    await settle();
    expect(host.commands.download).toHaveBeenCalledWith('/srv/share/report.pdf', '/home/beta');
  });

  it('marks an upload the host declined as refused, with that reason on its row', async () => {
    const host = unixHost();
    const fm = await opened(host);
    fm.select('local', 'notes.txt', { ctrl: false, shift: false });
    fm.transferSelection('local');
    await settle();
    host.status.uploads_refused += 1;
    await fm.poll();
    draw(fm);
    expect(text('fm-job-state')).toBe(t('en', 'fm.failure.declined'));
  });

  it('joins a Windows path with the separator that host uses', () => {
    expect(joinPath('C:\\Users\\beta', 'notes.txt')).toBe('C:\\Users\\beta\\notes.txt');
    expect(joinPath('C:\\', 'notes.txt')).toBe('C:\\notes.txt');
    expect(joinPath('/home/beta', 'notes.txt')).toBe('/home/beta/notes.txt');
    expect(joinPath('/', 'notes.txt')).toBe('/notes.txt');
  });
});

describe('changing files', () => {
  it('says why this machine refused a new folder', async () => {
    const host = unixHost();
    host.commands.localOp = vi.fn().mockRejectedValue({ code: 'EXISTS', message: '' });
    const fm = await opened(host);
    fm.startCreate('local');
    await fm.commitEdit('local', 'projects');
    expect(host.commands.localOp).toHaveBeenCalledWith({
      kind: 'make_dir',
      path: '/home/beta/projects',
    });
    expect(fm.notice).toEqual(expect.objectContaining({ text: t('en', 'fm.error.exists'), error: true }));
  });

  it('says why the host refused a rename, once its answer arrives', async () => {
    const host = unixHost();
    const fm = await opened(host);
    fm.select('remote', 'report.pdf', { ctrl: false, shift: false });
    fm.startRename('remote');
    await fm.commitEdit('remote', 'final.pdf');
    expect(host.commands.remoteOp).toHaveBeenCalledWith({
      kind: 'rename',
      path: '/srv/share/report.pdf',
      name: 'final.pdf',
    });
    expect(fm.notice).toBeNull();
    host.status.op_results = [{ id: 7, refused: 'busy' }];
    await fm.poll();
    expect(fm.notice?.text).toBe(t('en', 'fm.error.busy'));
  });

  it('deletes nothing until the second "yes"', async () => {
    const host = unixHost();
    const fm = await opened(host);
    fm.select('local', 'notes.txt', { ctrl: false, shift: false });
    fm.askDelete('local');
    draw(fm);
    expect(text('fm-confirm')).toContain(t('en', 'fm.confirmDelete.one', 'notes.txt'));
    expect(host.commands.localOp).not.toHaveBeenCalled();

    fm.cancelDelete();
    expect(host.commands.localOp).not.toHaveBeenCalled();

    fm.askDelete('local');
    await fm.confirmDelete();
    expect(host.commands.localOp).toHaveBeenCalledWith({
      kind: 'delete',
      path: '/home/beta/notes.txt',
    });
  });
});

describe('the window', () => {
  it('claims F5 so a reload cannot throw the window away mid-transfer', async () => {
    const host = unixHost();
    const fm = await opened(host);
    const before = vi.mocked(host.commands.localList).mock.calls.length;
    expect(fm.onKey(new KeyboardEvent('keydown', { key: 'F5' }))).toBe(true);
    expect(vi.mocked(host.commands.localList).mock.calls.length).toBe(before + 1);
  });

  it('says the session is gone rather than showing the last listing as current', async () => {
    const host = unixHost();
    const fm = await opened(host);
    host.commands.remoteStatus = vi.fn().mockRejectedValue({ code: 'NO_SESSION', message: '' });
    await fm.poll();
    draw(fm);
    expect(text('fm-lost')).toBe(t('en', 'fm.lost'));
  });

  it('offers Disconnect only when the window is the whole session', async () => {
    const fm = await opened();
    draw(fm, 'en', false);
    expect(container.querySelector('[data-testid="fm-disconnect"]')).toBeNull();
    draw(fm, 'en', true);
    expect(text('fm-disconnect')).toBe(t('en', 'fm.disconnect'));
  });
});

describe('accessibility', () => {
  const LAYOUT_DEPENDENT_RULES = ['color-contrast', 'target-size'];

  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations (${locale})`, async () => {
      const fm = await opened(fakeHost(() => ({ path: '/srv/share', truncated: true })));
      fm.select('local', 'notes.txt', { ctrl: false, shift: false });
      draw(fm, locale, true);
      const results = await axe.run(container, {
        rules: Object.fromEntries(LAYOUT_DEPENDENT_RULES.map((id) => [id, { enabled: false }])),
      });
      expect(results.violations).toEqual([]);
    });
  }
});
