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
  terminalChrome,
  TerminalSession,
  type TerminalCommands,
  type TerminalEvent,
  type TerminalScreen,
} from './terminal';

// `mountTerminal` loads the emulator itself, and a real `xterm.js` in jsdom
// would measure a font that is not there. These six calls are the whole of
// what it asks of one, so the stand-in is the whole of what it needs.
vi.mock('@xterm/xterm', () => ({
  Terminal: class {
    cols = 80;
    rows = 24;
    open(): void {}
    write(): void {}
    writeln(): void {}
    onData(): void {}
    onResize(): void {}
    dispose(): void {}
  },
}));
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
function screen(): TerminalScreen & { written: Uint8Array[]; lines: string[] } {
  const written: Uint8Array[] = [];
  const lines: string[] = [];
  return {
    written,
    lines,
    write: (data) => written.push(data),
    writeLine: (text) => lines.push(text),
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
  }
});
