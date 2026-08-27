// The file transfer panel (design doc §9.2, §15; ADR 0032).
//
// The panel's job is to make an incoming file a decision. So the tests are
// about the decision: an offer is never taken without a press, the answer
// carries the direction the button says, and the panel only exists for a
// session that actually holds `file_transfer` — a switch the host has not
// turned on must not have a "send a file" button under it.
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  fileTransferPanel,
  formatSize,
  percent,
  type FileCommands,
  type FileTransfers,
  type TransferRow,
} from './file-transfers';
import { SUPPORTED_LOCALES, t } from './i18n';
import { sessionStatus, type SessionStatus } from './session-status';

const invoke = vi.fn().mockResolvedValue(undefined);

vi.mock('@tauri-apps/api/core', () => ({
  invoke: (...args: unknown[]) => invoke(...args),
}));

const PEER = 'guest-ab12';

function commands(): FileCommands & {
  offer: ReturnType<typeof vi.fn>;
  accept: ReturnType<typeof vi.fn>;
  abort: ReturnType<typeof vi.fn>;
} {
  return {
    offer: vi.fn().mockResolvedValue(undefined),
    accept: vi.fn().mockResolvedValue(undefined),
    abort: vi.fn().mockResolvedValue(undefined),
    list: vi.fn().mockResolvedValue({ offers: [], transfers: [] }),
  };
}

const running: TransferRow = {
  peer_label: PEER,
  transfer_id: 4,
  name: 'report.pdf',
  size: 2048,
  moved: 512,
  incoming: true,
  state: 'running',
};

const session: SessionStatus = {
  peer_label: PEER,
  role: 'view_only',
  input: false,
  state: 'active',
  clipboard_read: false,
  clipboard_write: false,
  file_transfer: true,
  recording: false,
  recording_active: false,
  record_request: false,
};

let container: HTMLElement;

beforeEach(() => {
  invoke.mockClear();
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

function draw(data: FileTransfers, cmds: FileCommands): void {
  render(fileTransferPanel(PEER, data, 'en', cmds), container);
}

describe('an incoming offer', () => {
  it('is not taken until someone presses accept', () => {
    const cmds = commands();
    draw({ offers: [{ peer_label: PEER, name: 'report.pdf', size: 4096 }], transfers: [] }, cmds);

    expect(container.querySelector('[data-testid="file-offer"]')).not.toBeNull();
    expect(cmds.accept).not.toHaveBeenCalled();

    container.querySelector<HTMLButtonElement>('[data-testid="file-accept"]')?.click();
    expect(cmds.accept).toHaveBeenCalledWith(PEER, true);
  });

  it('carries the direction the button says when it is declined', () => {
    const cmds = commands();
    draw({ offers: [{ peer_label: PEER, name: 'report.pdf', size: 4096 }], transfers: [] }, cmds);
    container.querySelector<HTMLButtonElement>('[data-testid="file-decline"]')?.click();
    expect(cmds.accept).toHaveBeenCalledWith(PEER, false);
  });

  it('shows only the offers of this session', () => {
    const cmds = commands();
    draw(
      {
        offers: [
          { peer_label: PEER, name: 'mine.pdf', size: 1 },
          { peer_label: 'guest-9999', name: 'someone-elses.pdf', size: 1 },
        ],
        transfers: [],
      },
      cmds,
    );
    expect(container.querySelectorAll('[data-testid="file-offer"]')).toHaveLength(1);
    expect(container.textContent).toContain('mine.pdf');
    expect(container.textContent).not.toContain('someone-elses.pdf');
  });
});

describe('a running transfer', () => {
  it('can be cancelled by its own id', () => {
    const cmds = commands();
    draw({ offers: [], transfers: [running] }, cmds);
    container.querySelector<HTMLButtonElement>('[data-testid="file-cancel"]')?.click();
    expect(cmds.abort).toHaveBeenCalledWith(PEER, 4);
  });

  it('shows progress while running and an outcome once it has ended', () => {
    const cmds = commands();
    draw({ offers: [], transfers: [running] }, cmds);
    const bar = container.querySelector<HTMLProgressElement>('[data-testid="file-progress"]');
    expect(bar?.value).toBe(25);
    expect(container.querySelector('[data-testid="file-state"]')).toBeNull();

    draw({ offers: [], transfers: [{ ...running, moved: 2048, state: 'completed' }] }, cmds);
    expect(container.querySelector('[data-testid="file-progress"]')).toBeNull();
    expect(container.querySelector('[data-testid="file-state"]')?.textContent?.trim()).toBe('Done');
    // A cancel button on something that already ended would do nothing.
    expect(container.querySelector('[data-testid="file-cancel"]')).toBeNull();
  });
});

describe('the send button', () => {
  it('asks the Rust side to open the picker, and supplies no path itself', () => {
    const cmds = commands();
    draw({ offers: [], transfers: [] }, cmds);
    container.querySelector<HTMLButtonElement>('[data-testid="file-send"]')?.click();
    expect(cmds.offer).toHaveBeenCalledWith(PEER);
    expect(cmds.offer).toHaveBeenCalledTimes(1);
    // One argument only: the peer. A path would mean the webview had one.
    expect(cmds.offer.mock.calls[0]).toHaveLength(1);
  });
});

describe('the session row', () => {
  it('has no file panel until the host grants file transfer', () => {
    render(
      sessionStatus(
        [{ ...session, file_transfer: false }],
        'en',
        () => {},
        [],
        () => {},
        false,
        () => {},
        new Map(),
        { offers: [], transfers: [] },
        commands(),
      ),
      container,
    );
    expect(container.querySelector('[data-testid="file-panel"]')).toBeNull();

    render(
      sessionStatus(
        [session],
        'en',
        () => {},
        [],
        () => {},
        false,
        () => {},
        new Map(),
        { offers: [], transfers: [] },
        commands(),
      ),
      container,
    );
    expect(container.querySelector('[data-testid="file-panel"]')).not.toBeNull();
  });
});

describe('presentation', () => {
  it('renders sizes a person can read', () => {
    expect(formatSize(0)).toBe('0 B');
    expect(formatSize(512)).toBe('512 B');
    expect(formatSize(2048)).toBe('2 KB');
    expect(formatSize(1024 * 1024 * 3.5)).toBe('3.5 MB');
    expect(formatSize(-1)).toBe('0 B');
  });

  it('clamps progress, and counts an empty file as finished', () => {
    expect(percent({ ...running, moved: 0 })).toBe(0);
    expect(percent({ ...running, moved: 4096 })).toBe(100);
    expect(percent({ ...running, size: 0, moved: 0 })).toBe(100);
  });

  it('names every control in both locales and keeps them keyboard-reachable', () => {
    for (const locale of SUPPORTED_LOCALES) {
      render(
        fileTransferPanel(
          PEER,
          { offers: [{ peer_label: PEER, name: 'report.pdf', size: 10 }], transfers: [running] },
          locale,
          commands(),
        ),
        container,
      );
      for (const testid of ['file-send', 'file-accept', 'file-decline', 'file-cancel']) {
        const button = container.querySelector<HTMLButtonElement>(`[data-testid="${testid}"]`);
        expect(button, `${testid} missing in ${locale}`).not.toBeNull();
        expect(button?.tagName).toBe('BUTTON');
        expect(button?.getAttribute('aria-label')).toBeTruthy();
      }
      expect(container.textContent).toContain(t(locale, 'files.heading'));
    }
  });
});
