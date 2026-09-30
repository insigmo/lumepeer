// The guest's terminal window (design doc §4.1, §18; ADR 0079).
//
// A terminal is the one capability with no picture attached to it, so the
// tests are about the two things that makes load-bearing: an answer from the
// host always reaches the person who asked — an id or one of the four
// refusals of §18, never silence — and this window holds nothing. Output goes
// to the emulator and is not kept, which is §15 applied to the place it would
// be easiest to break.
import * as axe from 'axe-core';
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { SUPPORTED_LOCALES, t } from './i18n';
import {
  decodeTerminalPoll,
  mountTerminal,
  ownLine,
  terminalChrome,
  terminalClipboardKey,
  terminalMenu,
  TERMINAL_THEME,
  TerminalSession,
  type TerminalCommands,
  type TerminalEvent,
  type TerminalLineTone,
  type TerminalScreen,
} from './terminal';

/** What the last emulator was constructed with, loaded and handed. */
const emulator = vi.hoisted(() => ({
  options: undefined as unknown,
  addons: [] as unknown[],
  pasted: [] as string[],
  selection: '',
  fits: 0,
  /** What the fit addon would size the grid to: the room the window has. */
  room: { cols: 120, rows: 40 },
  /** The last `ResizeObserver` callback, for a test to call as a resize. */
  onResize: (() => {}) as () => void,
}));

// `mountTerminal` loads the emulator itself, and a real `xterm.js` in jsdom
// would measure a font that is not there. These calls are the whole of what
// it asks of one, so the stand-in is the whole of what it needs.
vi.mock('@xterm/xterm', () => ({
  Terminal: class {
    constructor(options: unknown) {
      emulator.options = options;
      emulator.addons = [];
      emulator.pasted = [];
    }
    cols = 80;
    rows = 24;
    open(): void {}
    write(): void {}
    writeln(): void {}
    onData(): void {}
    onResize(): void {}
    dispose(): void {}
    focus(): void {}
    loadAddon(addon: unknown): void {
      emulator.addons.push(addon);
    }
    attachCustomKeyEventHandler(): void {}
    hasSelection(): boolean {
      return emulator.selection !== '';
    }
    getSelection(): string {
      return emulator.selection;
    }
    selectAll(): void {}
    paste(text: string): void {
      emulator.pasted.push(text);
    }
  },
}));
vi.mock('@xterm/addon-fit', () => ({
  FitAddon: class {
    proposeDimensions(): { cols: number; rows: number } {
      return emulator.room;
    }
    fit(): void {
      emulator.fits += 1;
    }
  },
}));
// jsdom has no layout, so nothing would ever call it back on its own; a test
// that wants a resize calls `emulator.onResize`.
vi.stubGlobal(
  'ResizeObserver',
  class {
    constructor(callback: () => void) {
      emulator.onResize = callback;
    }
    observe(): void {}
    disconnect(): void {}
  },
);
const PEER = 'guest-ab12';

const EVENT_OUTPUT = 0;
const EVENT_OPENED = 1;
const EVENT_CLOSED = 2;

/** Builds one `terminal_poll` body the way the Rust side frames it. */
function body(records: { shell: number; event: number; payload?: Uint8Array }[]): ArrayBuffer {
  const total =
    2 + records.reduce((sum, record) => sum + 9 + (record.payload?.byteLength ?? 0), 0);
  const bytes = new Uint8Array(total);
  const view = new DataView(bytes.buffer);
  view.setUint16(0, records.length, true);
  let at = 2;
  for (const record of records) {
    const payload = record.payload ?? new Uint8Array(0);
    view.setUint32(at, record.shell, true);
    view.setUint8(at + 4, record.event);
    view.setUint32(at + 5, payload.byteLength, true);
    bytes.set(payload, at + 9);
    at += 9 + payload.byteLength;
  }
  return bytes.buffer;
}

function commands(): TerminalCommands & Record<keyof TerminalCommands, ReturnType<typeof vi.fn>> {
  return {
    open: vi.fn().mockResolvedValue(undefined),
    input: vi.fn().mockResolvedValue(undefined),
    resize: vi.fn().mockResolvedValue(undefined),
    close: vi.fn().mockResolvedValue(undefined),
    poll: vi.fn().mockResolvedValue(body([])),
  };
}

/** A screen that records rather than draws, so no emulator is needed. */
function screen(): TerminalScreen & {
  written: Uint8Array[];
  lines: string[];
  tones: TerminalLineTone[];
} {
  const written: Uint8Array[] = [];
  const lines: string[] = [];
  const tones: TerminalLineTone[] = [];
  return {
    written,
    lines,
    tones,
    write: (data) => written.push(data),
    writeLine: (text, tone) => {
      lines.push(text);
      tones.push(tone);
    },
  };
}

function text(chunks: readonly Uint8Array[]): string {
  return chunks.map((chunk) => new TextDecoder().decode(chunk)).join('');
}

describe('reading a poll', () => {
  it('reads back what the host framed, in order', () => {
    const decoded = decodeTerminalPoll(
      body([
        { shell: 7, event: EVENT_OPENED },
        { shell: 7, event: EVENT_OUTPUT, payload: new TextEncoder().encode('$ ') },
        { shell: 7, event: EVENT_CLOSED },
      ]),
    );
    expect(decoded.map((event) => event.kind)).toEqual(['opened', 'output', 'closed']);
    expect(decoded[0]).toEqual({ kind: 'opened', shell: 7 });
    expect(decoded[1]?.kind === 'output' && text([decoded[1].data])).toBe('$ ');
  });

  it('names each of the four refusals, and none of them names a shell (§18)', () => {
    for (const [event, reason] of [
      [3, 'not_granted'],
      [4, 'too_many'],
      [5, 'cannot_drop_privileges'],
      [6, 'unavailable'],
    ] as const) {
      expect(decodeTerminalPoll(body([{ shell: 0, event }]))).toEqual([
        { kind: 'refused', reason },
      ]);
    }
  });

  // A poll runs on a timer, so a body this build cannot read has to leave the
  // window able to ask again rather than throwing into the timer's callback.
  it('keeps the records that were whole and stops, rather than throwing', () => {
    const whole = new Uint8Array(
      body([
        { shell: 1, event: EVENT_OPENED },
        { shell: 1, event: EVENT_OUTPUT, payload: new TextEncoder().encode('hello') },
      ]),
    );
    expect(decodeTerminalPoll(whole.buffer.slice(0, whole.byteLength - 2))).toEqual([
      { kind: 'opened', shell: 1 },
    ]);
    expect(decodeTerminalPoll(new Uint8Array(0).buffer)).toEqual([]);
    // A count that promises more than the body carries is the same case.
    const lying = new Uint8Array(2);
    new DataView(lying.buffer).setUint16(0, 9, true);
    expect(decodeTerminalPoll(lying.buffer)).toEqual([]);
  });

  it('ignores an event byte this build has never heard of', () => {
    expect(decodeTerminalPoll(body([{ shell: 1, event: 99 }]))).toEqual([]);
  });
});

describe('one shell', () => {
  it('is the host that names it, and the window that repeats it back', async () => {
    const cmds = commands();
    const session = new TerminalSession(PEER, cmds, screen(), 'en');
    expect(session.shell).toBeNull();

    await session.open(80, 24);
    expect(cmds.open).toHaveBeenCalledWith(PEER, 80, 24);
    expect(session.status).toBe('opening');

    session.apply(decodeTerminalPoll(body([{ shell: 42, event: EVENT_OPENED }])));
    expect(session.shell).toBe(42);
    expect(session.status).toBe('open');

    await session.send('ls\r');
    expect(cmds.input).toHaveBeenCalledWith(PEER, 42, Array.from(new TextEncoder().encode('ls\r')));
    await session.resize(120, 40);
    expect(cmds.resize).toHaveBeenCalledWith(PEER, 42, 120, 40);
  });

  it('asks once while an ask is in flight', async () => {
    const cmds = commands();
    const session = new TerminalSession(PEER, cmds, screen(), 'en');
    await session.open(80, 24);
    await session.open(80, 24);
    expect(cmds.open).toHaveBeenCalledTimes(1);
  });

  it('types nothing before the host has named a shell', async () => {
    const cmds = commands();
    const session = new TerminalSession(PEER, cmds, screen(), 'en');
    await session.open(80, 24);
    await session.send('rm -rf /\r');
    await session.resize(100, 30);
    await session.close();
    expect(cmds.input).not.toHaveBeenCalled();
    expect(cmds.resize).not.toHaveBeenCalled();
    expect(cmds.close).not.toHaveBeenCalled();
  });

  // Every keystroke is its own IPC call, and calls made without waiting for
  // the one before race each other to the actor: a line typed quickly reached
  // a Linux host's bash scrambled.
  it('sends what was typed in the order it was typed, however fast', async () => {
    const cmds = commands();
    const started: string[] = [];
    const finish: Array<() => void> = [];
    cmds.input.mockImplementation((_peer: string, _shell: number, data: number[]) => {
      started.push(new TextDecoder().decode(new Uint8Array(data)));
      return new Promise<void>((resolve) => finish.push(resolve));
    });
    const session = new TerminalSession(PEER, cmds, screen(), 'en');
    session.apply(decodeTerminalPoll(body([{ shell: 1, event: EVENT_OPENED }])));
    const settle = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

    const sent = ['e', 'c', 'h', 'o'].map((key) => session.send(key));
    await settle();
    expect(started).toEqual(['e']);
    for (let next = finish.shift(); next; next = finish.shift()) {
      next();
      await settle();
    }
    await Promise.all(sent);
    expect(started).toEqual(['e', 'c', 'h', 'o']);
  });

  it('keeps typing after one keystroke could not be sent', async () => {
    const cmds = commands();
    cmds.input.mockRejectedValueOnce(new Error('the terminal channel is full'));
    const session = new TerminalSession(PEER, cmds, screen(), 'en');
    session.apply(decodeTerminalPoll(body([{ shell: 1, event: EVENT_OPENED }])));

    await expect(session.send('a')).rejects.toThrow('full');
    await session.send('b');
    expect(cmds.input).toHaveBeenLastCalledWith(PEER, 1, [98]);
  });

  it('draws its own shell and drops output addressed to another', () => {
    const sink = screen();
    const session = new TerminalSession(PEER, commands(), sink, 'en');
    session.apply(
      decodeTerminalPoll(
        body([
          { shell: 1, event: EVENT_OPENED },
          { shell: 1, event: EVENT_OUTPUT, payload: new TextEncoder().encode('mine') },
          { shell: 2, event: EVENT_OUTPUT, payload: new TextEncoder().encode('theirs') },
        ]),
      ),
    );
    expect(text(sink.written)).toBe('mine');
  });

  it('stops when the shell ends, and says so on screen', () => {
    const sink = screen();
    const session = new TerminalSession(PEER, commands(), sink, 'en');
    session.apply([
      { kind: 'opened', shell: 5 },
      { kind: 'closed', shell: 5 },
    ] satisfies TerminalEvent[]);
    expect(session.shell).toBeNull();
    expect(session.status).toBe('closed');
    expect(sink.lines).toContain(t('en', 'terminal.ended'));
    expect(sink.tones).toEqual(['note']);
  });

  it('ends the shell on the host when the window is done with it', async () => {
    const cmds = commands();
    const session = new TerminalSession(PEER, cmds, screen(), 'en');
    session.apply([{ kind: 'opened', shell: 3 }]);
    await session.close();
    expect(cmds.close).toHaveBeenCalledWith(PEER, 3);
    expect(session.shell).toBeNull();
  });
});

// §18: every refusal travels, and says which of the four it is. The one that
// matters most is `cannot_drop_privileges`, which is the host working exactly
// as intended rather than a malfunction (ADR 0079 decision 2).
describe('a refusal', () => {
  it('reaches the person who asked, in words, for each of the four', () => {
    for (const [event, key] of [
      [3, 'terminal.refused.notGranted'],
      [4, 'terminal.refused.tooMany'],
      [5, 'terminal.refused.cannotDropPrivileges'],
      [6, 'terminal.refused.unavailable'],
    ] as const) {
      const sink = screen();
      const session = new TerminalSession(PEER, commands(), sink, 'en');
      session.apply(decodeTerminalPoll(body([{ shell: 0, event }])));
      expect(session.status).toBe('refused');
      expect(session.shell).toBeNull();
      expect(sink.lines).toEqual([t('en', key)]);
      expect(sink.tones).toEqual(['error']);
    }
  });

  it('leaves the window able to ask again', async () => {
    const cmds = commands();
    const session = new TerminalSession(PEER, cmds, screen(), 'en');
    await session.open(80, 24);
    session.apply(decodeTerminalPoll(body([{ shell: 0, event: 4 }])));
    await session.open(80, 24);
    expect(cmds.open).toHaveBeenCalledTimes(2);
    expect(session.refusal).toBeNull();
  });
});

// ADR 0132: what this window says is told apart from what the shell says, and
// the shell's colours are the app's.
describe('how it looks', () => {
  it('sets its own lines apart and leaves no colour on for the shell', () => {
    const note = ownLine('Shell ended', 'note');
    // Reset first, whatever the shell left on; reset last, so its next
    // prompt starts plain.
    expect(note.startsWith('\r\n\x1b[0m')).toBe(true);
    expect(note.endsWith('Shell ended\x1b[0m')).toBe(true);
    expect(ownLine('Shell ended', 'error')).not.toBe(note);
  });

  it('draws the shell in the app palette', async () => {
    const root = document.createElement('div');
    const chrome = document.createElement('div');
    const controls = await mountTerminal(root, chrome, 'en', PEER, () => {}, commands());
    expect(emulator.options).toMatchObject({ theme: TERMINAL_THEME });
    controls.stop();
  });

  // The shell was 80×24 in a window of any size.
  it('sizes the shell to the window rather than to 80×24', async () => {
    const { FitAddon } = await import('@xterm/addon-fit');
    const controls = await mountTerminal(
      document.createElement('div'),
      document.createElement('div'),
      'en',
      PEER,
      () => {},
      commands(),
    );
    expect(emulator.addons.some((addon) => addon instanceof FitAddon)).toBe(true);
    controls.stop();
  });

  it('follows the window, but not down to no size at all', async () => {
    const controls = await mountTerminal(
      document.createElement('div'),
      document.createElement('div'),
      'en',
      PEER,
      () => {},
      commands(),
    );
    const before = emulator.fits;

    // Minimized: no room at all, which the addon floors at 2×1, so the shell
    // keeps the size it had.
    emulator.room = { cols: 2, rows: 1 };
    emulator.onResize();
    await new Promise((resolve) => requestAnimationFrame(resolve));
    expect(emulator.fits).toBe(before);

    emulator.room = { cols: 120, rows: 40 };
    emulator.onResize();
    await vi.waitFor(() => {
      expect(emulator.fits).toBe(before + 1);
    });
    controls.stop();
  });
});

// ADR 0132: a right click did nothing and nothing could be copied or pasted.
describe('copy and paste', () => {
  const key = (code: string, mods: Partial<KeyboardEvent> = {}) => ({
    type: 'keydown',
    code,
    ctrlKey: true,
    shiftKey: false,
    altKey: false,
    metaKey: false,
    ...mods,
  });

  it('keeps Ctrl+C the interrupt unless text is selected', () => {
    expect(terminalClipboardKey(key('KeyC'), false)).toBeNull();
    expect(terminalClipboardKey(key('KeyC'), true)).toBe('copy');
    expect(terminalClipboardKey(key('KeyC', { shiftKey: true }), false)).toBe('copy');
  });

  it('pastes on Ctrl+V and Ctrl+Shift+V, and leaves every other key to the shell', () => {
    expect(terminalClipboardKey(key('KeyV'), false)).toBe('paste');
    expect(terminalClipboardKey(key('KeyV', { shiftKey: true }), false)).toBe('paste');
    expect(terminalClipboardKey(key('KeyV', { ctrlKey: false }), false)).toBeNull();
    expect(terminalClipboardKey(key('KeyV', { altKey: true }), false)).toBeNull();
    expect(terminalClipboardKey(key('KeyD'), true)).toBeNull();
    expect(terminalClipboardKey(key('KeyC', { type: 'keyup' }), true)).toBeNull();
  });

  it('offers copy only when something is selected', () => {
    const host = document.createElement('div');
    const actions = { copy: vi.fn(), paste: vi.fn(), selectAll: vi.fn() };
    render(terminalMenu(null, 'en', false, actions), host);
    expect(host.querySelector('[data-testid="terminal-menu"]')).toBeNull();

    render(terminalMenu({ x: 10, y: 20, canCopy: false }, 'en', false, actions), host);
    const copy = host.querySelector<HTMLButtonElement>('[data-testid="terminal-menu-copy"]');
    expect(copy?.disabled).toBe(true);
    render(terminalMenu({ x: 10, y: 20, canCopy: true }, 'en', false, actions), host);
    expect(copy?.disabled).toBe(false);
    copy?.click();
    host.querySelector<HTMLButtonElement>('[data-testid="terminal-menu-paste"]')?.click();
    host.querySelector<HTMLButtonElement>('[data-testid="terminal-menu-select-all"]')?.click();
    expect([actions.copy, actions.paste, actions.selectAll].map((f) => f.mock.calls.length)).toEqual([
      1, 1, 1,
    ]);
  });

  it('opens on a right click, pastes what the clipboard holds, and closes', async () => {
    const readText = vi.fn().mockResolvedValue('ls -la');
    // jsdom has no clipboard at all.
    Object.defineProperty(navigator, 'clipboard', {
      value: { readText, writeText: vi.fn() },
      configurable: true,
    });
    const root = document.createElement('div');
    document.body.append(root);
    const controls = await mountTerminal(root, document.createElement('div'), 'en', PEER, () => {}, commands());

    root.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 5, clientY: 5 }));
    const paste = document.querySelector<HTMLButtonElement>('[data-testid="terminal-menu-paste"]');
    expect(paste).not.toBeNull();
    paste?.click();
    await vi.waitFor(() => {
      expect(emulator.pasted).toEqual(['ls -la']);
    });
    expect(document.querySelector('[data-testid="terminal-menu"]')).toBeNull();

    controls.stop();
    root.remove();
    Reflect.deleteProperty(navigator, 'clipboard');
  });

  it('closes on Escape without doing anything', async () => {
    const root = document.createElement('div');
    document.body.append(root);
    const controls = await mountTerminal(root, document.createElement('div'), 'en', PEER, () => {}, commands());
    root.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }));
    expect(document.querySelector('[data-testid="terminal-menu"]')).not.toBeNull();
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
    expect(document.querySelector('[data-testid="terminal-menu"]')).toBeNull();
    controls.stop();
    root.remove();
  });
});

// §18 item 1: the Close button. It is the only way out of a panel whose host
// said no, so it is never disabled and it closes the terminal rather than
// merely ending a shell that may never have started.
describe('the close button', () => {
  let root: HTMLElement;
  let chrome: HTMLElement;

  beforeEach(() => {
    root = document.createElement('div');
    chrome = document.createElement('div');
    document.body.append(root, chrome);
  });

  afterEach(() => {
    root.remove();
    chrome.remove();
  });

  function button(): HTMLButtonElement | null {
    return chrome.querySelector<HTMLButtonElement>('[data-testid="terminal-close"]');
  }

  const STATUSES = ['idle', 'opening', 'open', 'closed', 'refused'] as const;

  it('is active in every state the panel can be in', () => {
    for (const status of STATUSES) {
      render(terminalChrome(status, 'en', () => {}), chrome);
      expect(button()?.disabled, `disabled while ${status}`).toBe(false);
    }
  });

  // The status dot is coloured from this attribute (ADR 0132).
  it('names its state for the status dot', () => {
    for (const status of STATUSES) {
      render(terminalChrome(status, 'en', () => {}), chrome);
      expect(
        chrome.querySelector('[data-testid="terminal-state"]')?.getAttribute('data-status'),
      ).toBe(status);
    }
  });

  // The case the person actually hit: the host refused, no shell was ever
  // named, and the button is the only thing left that can dismiss the panel.
  it('closes the terminal after a refusal, with no shell to end', async () => {
    const cmds = commands();
    const onClose = vi.fn();
    const controls = await mountTerminal(root, chrome, 'en', PEER, onClose, cmds);
    cmds.poll.mockResolvedValue(body([{ shell: 0, event: 5 }]));

    await vi.waitFor(() => {
      expect(chrome.querySelector('[data-testid="terminal-state"]')?.textContent?.trim()).toBe(
        t('en', 'terminal.refusedHeading'),
      );
    });
    button()?.click();
    await vi.waitFor(() => {
      expect(onClose).toHaveBeenCalledTimes(1);
    });
    expect(cmds.close).not.toHaveBeenCalled();
    controls.stop();
  });

  // ADR 0079: the shell dies on the host before this side stops looking at
  // it, never the other way round.
  it('ends the shell on the host before it closes the terminal', async () => {
    const order: string[] = [];
    const cmds = commands();
    cmds.close.mockImplementation(() => {
      order.push('terminal_close');
      return Promise.resolve();
    });
    const onClose = vi.fn(() => {
      order.push('onClose');
    });
    const controls = await mountTerminal(root, chrome, 'en', PEER, onClose, cmds);
    cmds.poll.mockResolvedValue(body([{ shell: 11, event: EVENT_OPENED }]));

    await vi.waitFor(() => {
      expect(button()?.disabled).toBe(false);
      expect(chrome.querySelector('[data-testid="terminal-state"]')?.textContent?.trim()).toBe(
        t('en', 'terminal.running'),
      );
    });
    button()?.click();
    await vi.waitFor(() => {
      expect(onClose).toHaveBeenCalledTimes(1);
    });
    expect(cmds.close).toHaveBeenCalledWith(PEER, 11);
    expect(order).toEqual(['terminal_close', 'onClose']);
    controls.stop();
  });
});

describe('accessibility', () => {
  const LAYOUT_DEPENDENT_RULES = ['color-contrast', 'target-size'];
  let container: HTMLElement;

  beforeEach(() => {
    container = document.createElement('div');
    document.body.appendChild(container);
  });

  afterEach(() => {
    container.remove();
  });

  for (const locale of SUPPORTED_LOCALES) {
    it(`has no axe violations, and the close button is reachable (${locale})`, async () => {
      render(terminalChrome('open', locale, () => {}), container);
      const results = await axe.run(container, {
        rules: Object.fromEntries(LAYOUT_DEPENDENT_RULES.map((id) => [id, { enabled: false }])),
      });
      expect(results.violations).toEqual([]);
      expect(container.querySelectorAll('[tabindex]')).toHaveLength(0);
      const button = container.querySelector<HTMLButtonElement>('[data-testid="terminal-close"]');
      expect(button, `terminal-close missing in ${locale}`).not.toBeNull();
      expect(button?.disabled).toBe(false);
    });

    it(`has an accessible right-click menu (${locale})`, async () => {
      const actions = { copy: () => {}, paste: () => {}, selectAll: () => {} };
      render(terminalMenu({ x: 0, y: 0, canCopy: true }, locale, false, actions), container);
      const results = await axe.run(container, {
        rules: Object.fromEntries(LAYOUT_DEPENDENT_RULES.map((id) => [id, { enabled: false }])),
      });
      expect(results.violations).toEqual([]);
      for (const item of container.querySelectorAll('[role="menuitem"]')) {
        expect(item.textContent?.trim(), `an empty menu item in ${locale}`).not.toBe('');
      }
    });
  }
});
