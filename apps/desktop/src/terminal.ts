// The guest's terminal window (design doc §4.1; ADR 0079).
//
// Two halves, and the split is the point. Everything with a decision in it —
// what a poll's bytes mean, what state one shell is in, what a refusal says —
// is plain data and plain functions, so the tests drive it without a DOM.
// Rendering is `xterm.js` and nothing else: writing an ANSI emulator is not
// this project's work, and a half-written one is a rendering bug that looks
// like a remote-execution bug (ADR 0079 decision 6).
//
// Nothing here decides anything either. The host re-reads its own `terminal`
// grant for **every** shell this asks for and can take it back mid-command;
// what this window does with a refusal is say so, and what it does with a
// close is stop drawing.
//
// What was typed and what came back stay in this window. They are not logged,
// not stored, and not put anywhere a later session could read them (§15;
// ADR 0041) — the same rule that keeps clipboard contents out of the audit
// log.

import { html, render, type TemplateResult } from 'lit-html';

import type { Locale, TranslationKey } from './i18n';
import { t } from './i18n';

/**
 * How much history one shell keeps in this window, in lines.
 *
 * The emulator's own bound, on this side only: the host keeps no transcript
 * of anything (§15, ADR 0041), so there is nothing on that side to scroll
 * back through. A thousand lines is a build log's worth without holding a
 * whole session in a webview. Its counterpart in Rust bounds the *bytes*
 * queued for a window that has stopped polling — see
 * `crates/core/src/constants.rs::TERMINAL_SCROLLBACK_BYTES`.
 */
export const TERMINAL_SCROLLBACK_LINES = 1000;

/**
 * How often the window asks what its shells have said.
 *
 * A terminal is read by a person, so the bar is "does not feel laggy" rather
 * than a frame rate; each poll returns everything since the last one, so a
 * slower interval costs latency and never content.
 */
export const TERMINAL_POLL_MS = 50;

/** Why a host would not start a shell (§18; ADR 0079). */
export type TerminalRefusal =
  | 'not_granted'
  | 'too_many'
  | 'cannot_drop_privileges'
  | 'unavailable';

/** One thing a poll said happened, in the order it happened. */
export type TerminalEvent =
  | { kind: 'output'; shell: number; data: Uint8Array }
  | { kind: 'opened'; shell: number }
  | { kind: 'closed'; shell: number }
  | { kind: 'refused'; reason: TerminalRefusal };

/** How this panel talks to Tauri; injectable so the logic is testable. */
export interface TerminalCommands {
  open(peer: string, cols: number, rows: number): Promise<void>;
  input(peer: string, shell: number, data: number[]): Promise<void>;
  resize(peer: string, shell: number, cols: number, rows: number): Promise<void>;
  close(peer: string, shell: number): Promise<void>;
  /** The raw poll body; see {@link decodeTerminalPoll} for its layout. */
  poll(peer: string): Promise<ArrayBuffer>;
}

/** Default binding to the real IPC surface. */
export const tauriTerminalCommands: TerminalCommands = {
  async open(peer, cols, rows) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('terminal_open', { args: { peer, cols, rows } });
  },
  async input(peer, shell, data) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('terminal_input', { args: { peer, shell, data } });
  },
  async resize(peer, shell, cols, rows) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('terminal_resize', { args: { peer, shell, cols, rows } });
  },
  async close(peer, shell) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke('terminal_close', { args: { peer, shell } });
  },
  async poll(peer) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<ArrayBuffer>('terminal_poll', { args: { peer } });
  },
};

/** Header of one poll record: `shell:u32 | event:u8 | length:u32`. */
const RECORD_HEADER_BYTES = 4 + 1 + 4;
/** Size of the `count:u16` the body starts with. */
const COUNT_BYTES = 2;

const EVENT_OUTPUT = 0;
const EVENT_OPENED = 1;
const EVENT_CLOSED = 2;

/**
 * The four refusals, by the event byte each travels as.
 *
 * A table rather than a `switch` so that an event byte this build has never
 * heard of falls out as "not a refusal" instead of being guessed at.
 */
const REFUSALS: Readonly<Record<number, TerminalRefusal>> = {
  3: 'not_granted',
  4: 'too_many',
  5: 'cannot_drop_privileges',
  6: 'unavailable',
};

/** What each refusal says to the person who asked for the shell (§18). */
const REFUSAL_KEYS: Readonly<Record<TerminalRefusal, TranslationKey>> = {
  not_granted: 'terminal.refused.notGranted',
  too_many: 'terminal.refused.tooMany',
  cannot_drop_privileges: 'terminal.refused.cannotDropPrivileges',
  unavailable: 'terminal.refused.unavailable',
};

/**
 * Reads one `terminal_poll` body (ADR 0079).
 *
 * Little endian, and the same self-describing shape `view_cursor` uses:
 * `count:u16`, then `count` records of `shell:u32 | event:u8 | length:u32 |
 * payload`. Binary rather than JSON because a terminal produces bytes, and a
 * `Vec<u8>` through the JSON side of the IPC boundary is one number per byte.
 *
 * A truncated or unrecognized body yields the records that were whole and
 * stops, rather than throwing: this runs on a poll timer, and an exception
 * here would stop a window from ever reading its shell again.
 */
export function decodeTerminalPoll(buffer: ArrayBufferLike): TerminalEvent[] {
  const bytes = new Uint8Array(buffer);
  const events: TerminalEvent[] = [];
  if (bytes.byteLength < COUNT_BYTES) {
    return events;
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const count = view.getUint16(0, true);
  let at = COUNT_BYTES;
  for (let i = 0; i < count; i += 1) {
    if (at + RECORD_HEADER_BYTES > bytes.byteLength) {
      break;
    }
    const shell = view.getUint32(at, true);
    const event = view.getUint8(at + 4);
    const length = view.getUint32(at + 5, true);
    at += RECORD_HEADER_BYTES;
    if (at + length > bytes.byteLength) {
      break;
    }
    const payload = bytes.subarray(at, at + length);
    at += length;
    const refusal = REFUSALS[event];
    if (refusal) {
      events.push({ kind: 'refused', reason: refusal });
    } else if (event === EVENT_OUTPUT) {
      events.push({ kind: 'output', shell, data: payload });
    } else if (event === EVENT_OPENED) {
      events.push({ kind: 'opened', shell });
    } else if (event === EVENT_CLOSED) {
      events.push({ kind: 'closed', shell });
    }
  }
  return events;
}

/** Where a session draws; `xterm.js` in the window, a fake in the tests. */
export interface TerminalScreen {
  /** Raw bytes from the shell, escape sequences and all. */
  write(data: Uint8Array): void;
  /** A line this window wrote itself — a refusal, or "the shell ended". */
  writeLine(text: string): void;
}

/** What the panel is showing right now. */
export type TerminalStatus = 'idle' | 'opening' | 'open' | 'closed' | 'refused';

/**
 * One terminal window's state machine (ADR 0079).
 *
 * Holds no bytes: output goes straight to the screen and is not kept, which
 * is the §15 rule applied to the one place it would be easiest to break. What
 * it does hold is which shell the host named, so a keystroke can say which
 * shell it is for.
 */
export class TerminalSession {
  #shell: number | null = null;
  #status: TerminalStatus = 'idle';
  #refusal: TerminalRefusal | null = null;

  constructor(
    private readonly peer: string,
    private readonly commands: TerminalCommands,
    private readonly screen: TerminalScreen,
    private readonly locale: Locale,
  ) {}

  /** The id the host gave this shell, or `null` before it named one. */
  get shell(): number | null {
    return this.#shell;
  }

  get status(): TerminalStatus {
    return this.#status;
  }

  /** Why the host said no, when it did. */
  get refusal(): TerminalRefusal | null {
    return this.#refusal;
  }

  /**
   * Asks the host for a shell.
   *
   * Nothing is granted by asking: the answer arrives through a later poll as
   * an `opened` or one of the four refusals, and a second ask while one is in
   * flight is dropped rather than queued.
   */
  async open(cols: number, rows: number): Promise<void> {
    if (this.#status === 'opening' || this.#status === 'open') {
      return;
    }
    this.#status = 'opening';
    this.#refusal = null;
    await this.commands.open(this.peer, cols, rows);
  }

  /**
   * Applies everything one poll returned, in order.
   *
   * Output for a shell this window does not know is dropped: the open answer
   * and the first bytes travel on two different connections, and a window
   * must not draw a prompt it cannot type into.
   */
  apply(events: readonly TerminalEvent[]): void {
    for (const event of events) {
      switch (event.kind) {
        case 'opened':
          this.#shell = event.shell;
          this.#status = 'open';
          break;
        case 'output':
          if (event.shell === this.#shell) {
            this.screen.write(event.data);
          }
          break;
        case 'closed':
          if (event.shell === this.#shell) {
            this.#shell = null;
            this.#status = 'closed';
            this.screen.writeLine(t(this.locale, 'terminal.ended'));
          }
          break;
        case 'refused':
          this.#status = 'refused';
          this.#refusal = event.reason;
          this.screen.writeLine(t(this.locale, REFUSAL_KEYS[event.reason]));
          break;
      }
    }
  }

  /** What was typed, on its way to the shell. */
  async send(data: string): Promise<void> {
    const shell = this.#shell;
    if (shell === null) {
      return;
    }
    await this.commands.input(this.peer, shell, Array.from(new TextEncoder().encode(data)));
  }

  /** This window changed size, so the shell's own idea of it must too. */
  async resize(cols: number, rows: number): Promise<void> {
    const shell = this.#shell;
    if (shell === null) {
      return;
    }
    await this.commands.resize(this.peer, shell, cols, rows);
  }

  /** Done with this shell. The host kills the process and frees the pty. */
  async close(): Promise<void> {
    const shell = this.#shell;
    if (shell === null) {
      return;
    }
    this.#shell = null;
    this.#status = 'closed';
    await this.commands.close(this.peer, shell);
  }
}

/** What one line of this panel's chrome says, given a session's state. */
export function terminalStatusKey(status: TerminalStatus): TranslationKey {
  switch (status) {
    case 'opening':
      return 'terminal.opening';
    case 'open':
      return 'terminal.running';
    case 'closed':
      return 'terminal.ended';
    case 'refused':
      return 'terminal.refusedHeading';
    case 'idle':
      return 'terminal.idle';
  }
}

/**
 * The panel's chrome: a heading, a line saying what is happening, and the one
 * button that ends the shell.
 *
 * The emulator itself is not in here — `xterm.js` owns its own element and
 * would be destroyed by a re-render, so the host element is created once and
 * this draws around it.
 */
export function terminalChrome(
  status: TerminalStatus,
  locale: Locale,
  onClose: () => void,
): TemplateResult {
  return html`
    <div class="term-head">
      <h3 id="term-heading">${t(locale, 'terminal.heading')}</h3>
      <span class="term-state" role="status" data-testid="terminal-state"
        >${t(locale, terminalStatusKey(status))}</span
      >
      <button
        type="button"
        class="term-close"
        data-testid="terminal-close"
        ?disabled=${status !== 'open'}
        @click=${onClose}
      >
        ${t(locale, 'terminal.close')}
      </button>
    </div>
  `;
}

/**
 * Everything a mounted terminal hands back to the window around it.
 *
 * `stop` exists because this owns a poll timer and an emulator, and a view
 * window that closed while a shell was running must leave neither behind —
 * the same reason the host kills the process at its end of the same session.
 */
export interface TerminalControls {
  /** Ask for a shell, sized to the element the emulator is drawn in. */
  start(): Promise<void>;
  /** Tell the host this window changed size. */
  refit(): void;
  /** Stop polling, close the shell and dispose the emulator. */
  stop(): void;
}

/**
 * Mounts the terminal into `root` and starts its poll loop (ADR 0079).
 *
 * The emulator is loaded on first use rather than with the window: a session
 * that never opens a terminal should not pay for one, and this is a view
 * window whose job is the picture.
 */
export async function mountTerminal(
  root: HTMLElement,
  chrome: HTMLElement,
  locale: Locale,
  peer: string,
  commands: TerminalCommands = tauriTerminalCommands,
): Promise<TerminalControls> {
  const { Terminal } = await import('@xterm/xterm');
  const emulator = new Terminal({
    scrollback: TERMINAL_SCROLLBACK_LINES,
    convertEol: false,
    // Announcing every byte would make a build log unusable with a screen
    // reader; announcing the line the cursor is on is what a terminal is.
    screenReaderMode: true,
  });
  emulator.open(root);

  const screen: TerminalScreen = {
    write: (data) => emulator.write(data),
    writeLine: (text) => emulator.writeln(`\r\n${text}`),
  };
  const session = new TerminalSession(peer, commands, screen, locale);
  const draw = (): void => {
    render(
      terminalChrome(session.status, locale, () => {
        void session.close().catch(reportFailure('the shell could not be closed'));
      }),
      chrome,
    );
  };
  draw();

  emulator.onData((data) => {
    void session.send(data).catch(reportFailure('what was typed could not be sent'));
  });
  emulator.onResize(({ cols, rows }) => {
    void session.resize(cols, rows).catch(reportFailure('the shell could not be resized'));
  });

  const timer = setInterval(() => {
    void commands
      .poll(peer)
      .then((body) => {
        const before = session.status;
        session.apply(decodeTerminalPoll(body));
        if (session.status !== before) {
          draw();
        }
      })
      .catch(reportFailure('the shell could not be read'));
  }, TERMINAL_POLL_MS);

  return {
    async start(): Promise<void> {
      await session.open(emulator.cols, emulator.rows);
      draw();
    },
    refit(): void {
      void session.resize(emulator.cols, emulator.rows).catch(reportFailure('the shell could not be resized'));
    },
    stop(): void {
      clearInterval(timer);
      void session.close().catch(reportFailure('the shell could not be closed'));
      emulator.dispose();
    },
  };
}

/** One place for "the call failed", so no rejection is swallowed silently. */
function reportFailure(what: string): (error: unknown) => void {
  return (error: unknown) => {
    console.error(`${what}:`, error);
  };
}
