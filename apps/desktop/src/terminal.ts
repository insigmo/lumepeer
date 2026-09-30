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

import type { ITheme } from '@xterm/xterm';
import { html, nothing, render, type TemplateResult } from 'lit-html';
import { styleMap } from 'lit-html/directives/style-map.js';

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

/**
 * The emulator's colours: the app's dark palette, as the sixteen colours a
 * shell asks for by number (ADR 0132).
 *
 * Dark whatever the app's theme is, because the view window is: the terminal
 * sits on a video picture or fills a window of its own, and in both it is the
 * panel's black. Every colour but `black` reads at 4.5:1 or better on the
 * background; `black` is the one programs paint *behind* text with.
 */
export const TERMINAL_THEME: ITheme = {
  background: '#0e1115',
  foreground: '#e9ecf2',
  cursor: '#8aa9ff',
  cursorAccent: '#0e1115',
  selectionBackground: 'rgba(110, 145, 245, 0.35)',
  black: '#2a303b',
  red: '#ff6d7b',
  green: '#3ccf8e',
  yellow: '#f2b441',
  blue: '#6f95ff',
  magenta: '#c68cf5',
  cyan: '#46c6d6',
  white: '#c3c9d4',
  brightBlack: '#7a8293',
  brightRed: '#ff929c',
  brightGreen: '#62dca5',
  brightYellow: '#f7ca72',
  brightBlue: '#8aa9ff',
  brightMagenta: '#d8aefc',
  brightCyan: '#79dbe7',
  brightWhite: '#f5f7fa',
};

/** The app's `--font-mono`, which `xterm.js` cannot read from CSS. */
const TERMINAL_FONT = "ui-monospace, 'Cascadia Mono', 'SF Mono', Menlo, Consolas, monospace";

/**
 * What a line this window wrote itself is: news about the shell, or a refusal
 * (ADR 0132).
 */
export type TerminalLineTone = 'note' | 'error';

/**
 * The SGR each tone is drawn in: grey italic for news, bold red for a refusal.
 * Numbered colours rather than literal ones, so they come from
 * {@link TERMINAL_THEME} like everything the shell draws.
 */
const LINE_STYLE: Readonly<Record<TerminalLineTone, string>> = {
  note: '\x1b[3;90m',
  error: '\x1b[1;31m',
};

/**
 * One line this window wrote itself, set apart from the shell's (ADR 0132).
 *
 * The reset comes first because the shell may have left a colour on that ours
 * must not inherit, and last so that nothing the shell draws next inherits
 * ours.
 */
export function ownLine(text: string, tone: TerminalLineTone): string {
  return `\r\n\x1b[0m${LINE_STYLE[tone]}${text}\x1b[0m`;
}

/** Where a session draws; `xterm.js` in the window, a fake in the tests. */
export interface TerminalScreen {
  /** Raw bytes from the shell, escape sequences and all. */
  write(data: Uint8Array): void;
  /** A line this window wrote itself — a refusal, or "the shell ended". */
  writeLine(text: string, tone: TerminalLineTone): void;
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
  /** The keystroke on its way to the host that the next one waits for. */
  #sending: Promise<void> = Promise.resolve();

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
            this.screen.writeLine(t(this.locale, 'terminal.ended'), 'note');
          }
          break;
        case 'refused':
          this.#status = 'refused';
          this.#refusal = event.reason;
          this.screen.writeLine(t(this.locale, REFUSAL_KEYS[event.reason]), 'error');
          break;
      }
    }
  }

  /**
   * What was typed, on its way to the shell, in the order it was typed.
   *
   * Each keystroke is its own IPC call, and calls made without waiting for
   * the one before race each other to the actor: typed quickly, `echo` could
   * reach the shell as `ohce`. So each waits for the last, failed or not.
   */
  async send(data: string): Promise<void> {
    const shell = this.#shell;
    if (shell === null) {
      return;
    }
    const bytes = Array.from(new TextEncoder().encode(data));
    const sent = this.#sending.then(() => this.commands.input(this.peer, shell, bytes));
    this.#sending = sent.catch(() => undefined);
    await sent;
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
 * button that closes the terminal.
 *
 * The button is never disabled. What the panel is doing is what `term-state`
 * says; a button greyed out because the host refused is a button that cannot
 * dismiss the refusal, which is the state a person most wants out of. What
 * `onClose` does is the window's decision, not this function's — see
 * {@link mountTerminal}.
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
      <span class="term-state" role="status" data-testid="terminal-state" data-status=${status}
        >${t(locale, terminalStatusKey(status))}</span
      >
      <button
        type="button"
        class="term-close"
        data-testid="terminal-close"
        @click=${onClose}
      >
        ${t(locale, 'terminal.close')}
      </button>
    </div>
  `;
}

/** What a key does to the clipboard instead of reaching the shell (ADR 0132). */
export type TerminalClipboardKey = 'copy' | 'paste' | null;

/**
 * Which keys copy and paste rather than go to the shell, on a guest whose
 * shortcuts use Ctrl (ADR 0132).
 *
 * `Ctrl+C` stays the interrupt it is in every terminal, unless text is
 * selected — then it copies, as it does in Windows Terminal. `Ctrl+V` pastes;
 * the `^V` it would otherwise send is a shell's quoted-insert, which nobody
 * reaches for in a remote window. `Ctrl+Shift+C`/`V` always copy and paste,
 * as in a Linux terminal. Physical keys rather than characters, so the
 * shortcuts work under a Russian layout too. A Mac guest never gets here: its
 * `⌘C`/`⌘V` are not keys the emulator sends, and the webview copies and
 * pastes them itself.
 */
export function terminalClipboardKey(
  event: Pick<KeyboardEvent, 'type' | 'code' | 'ctrlKey' | 'shiftKey' | 'altKey' | 'metaKey'>,
  hasSelection: boolean,
): TerminalClipboardKey {
  if (event.type !== 'keydown' || !event.ctrlKey || event.altKey || event.metaKey) {
    return null;
  }
  if (event.code === 'KeyC' && (event.shiftKey || hasSelection)) {
    return 'copy';
  }
  if (event.code === 'KeyV') {
    return 'paste';
  }
  return null;
}

/** Where the terminal's own menu is open, and whether there is anything to copy. */
export interface TerminalMenuState {
  x: number;
  y: number;
  canCopy: boolean;
}

/** What the menu's three items do. */
export interface TerminalMenuActions {
  copy(): void;
  paste(): void;
  selectAll(): void;
}

/**
 * The menu a right click opens over the terminal (ADR 0132).
 *
 * The window suppresses the webview's own menu everywhere — over the picture
 * a right click belongs to the host — so without this one a right click in
 * the shell did nothing at all. Copy is disabled rather than hidden when
 * nothing is selected, so the menu keeps its shape. The shortcuts beside the
 * items are the ones {@link terminalClipboardKey} answers to.
 */
export function terminalMenu(
  menu: TerminalMenuState | null,
  locale: Locale,
  mac: boolean,
  actions: TerminalMenuActions,
): TemplateResult | typeof nothing {
  if (!menu) {
    return nothing;
  }
  const modifier = mac ? '⌘' : 'Ctrl+';
  return html`
    <div
      class="term-menu"
      role="menu"
      data-testid="terminal-menu"
      style=${styleMap({ left: `${menu.x}px`, top: `${menu.y}px` })}
    >
      <button
        type="button"
        role="menuitem"
        class="term-menu-item"
        data-testid="terminal-menu-copy"
        ?disabled=${!menu.canCopy}
        @click=${actions.copy}
      >
        <span>${t(locale, 'terminal.menu.copy')}</span><kbd>${modifier}C</kbd>
      </button>
      <button
        type="button"
        role="menuitem"
        class="term-menu-item"
        data-testid="terminal-menu-paste"
        @click=${actions.paste}
      >
        <span>${t(locale, 'terminal.menu.paste')}</span><kbd>${modifier}V</kbd>
      </button>
      <button
        type="button"
        role="menuitem"
        class="term-menu-item"
        data-testid="terminal-menu-select-all"
        @click=${actions.selectAll}
      >
        <span>${t(locale, 'terminal.menu.selectAll')}</span>
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
 *
 * `onClose` is what the Close button does once the shell is gone — close the
 * window, or hide the panel, depending on which of the two this window is
 * (ADR 0101). The choice belongs to the window and the shell belongs here, so
 * the order is fixed here and only the second half is handed out: the host is
 * told to end the shell first, and a shell left running on somebody else's
 * machine by a window that closed is the one outcome ADR 0079 may not produce.
 */
export async function mountTerminal(
  root: HTMLElement,
  chrome: HTMLElement,
  locale: Locale,
  peer: string,
  onClose: () => void,
  commands: TerminalCommands = tauriTerminalCommands,
): Promise<TerminalControls> {
  const [{ Terminal }, { FitAddon }] = await Promise.all([
    import('@xterm/xterm'),
    import('@xterm/addon-fit'),
  ]);
  const emulator = new Terminal({
    scrollback: TERMINAL_SCROLLBACK_LINES,
    convertEol: false,
    // Announcing every byte would make a build log unusable with a screen
    // reader; announcing the line the cursor is on is what a terminal is.
    screenReaderMode: true,
    theme: TERMINAL_THEME,
    fontFamily: TERMINAL_FONT,
    fontSize: 13,
    lineHeight: 1.2,
    cursorBlink: true,
    cursorStyle: 'bar',
  });
  // The shell is as many columns and rows as the window has room for, and
  // follows it when it changes size (ADR 0132). Without this it was the
  // emulator's default 80×24 in whatever size of window it sat in. A fit is
  // at most once a frame, because dragging a window edge resizes on every
  // pixel and each fit that changes the grid is a resize sent to the host.
  const fit = new FitAddon();
  emulator.loadAddon(fit);
  emulator.open(root);
  // A window laid out at no size at all — minimized, or not yet shown — is
  // not a size to give the shell. The addon floors "no room" at 2×1, and a
  // shell handed that redraws its prompt two characters to a line into the
  // scrollback. So the floor itself is the signal to leave the size alone.
  const fitToWindow = (): void => {
    const room = fit.proposeDimensions();
    if (room && room.cols > 2 && room.rows > 1) {
      fit.fit();
    }
  };
  fitToWindow();
  let fitting = 0;
  const resizer = new ResizeObserver(() => {
    cancelAnimationFrame(fitting);
    fitting = requestAnimationFrame(fitToWindow);
  });
  resizer.observe(root);

  // Copy and paste (ADR 0132). The keys are left to the webview rather than
  // done here: the emulator already answers the webview's own `copy` and
  // `paste` events, bracketed paste included, and a paste the webview does
  // itself needs no permission to read the clipboard. The menu has no such
  // event to ride on, so it goes through the clipboard API.
  const mac = /Macintosh|Mac OS X/.test(navigator.userAgent);
  emulator.attachCustomKeyEventHandler(
    (event) => mac || terminalClipboardKey(event, emulator.hasSelection()) === null,
  );
  const menuHost = document.createElement('div');
  document.body.append(menuHost);
  let menu: TerminalMenuState | null = null;
  const drawMenu = (): void => {
    render(terminalMenu(menu, locale, mac, menuActions), menuHost);
  };
  const closeMenu = (): void => {
    if (menu) {
      menu = null;
      drawMenu();
      emulator.focus();
    }
  };
  const menuActions: TerminalMenuActions = {
    copy: () => {
      const text = emulator.getSelection();
      closeMenu();
      void navigator.clipboard.writeText(text).catch(reportFailure('the selection could not be copied'));
    },
    paste: () => {
      closeMenu();
      void navigator.clipboard
        .readText()
        .then((text) => emulator.paste(text))
        .catch(reportFailure('the clipboard could not be pasted'));
    },
    selectAll: () => {
      closeMenu();
      emulator.selectAll();
    },
  };
  root.addEventListener('contextmenu', (event) => {
    event.preventDefault();
    menu = { x: event.clientX, y: event.clientY, canCopy: emulator.hasSelection() };
    drawMenu();
    // Kept inside the window: opened near an edge, it moves in rather than
    // being cut off.
    const shown = menuHost.querySelector<HTMLElement>('.term-menu');
    if (shown) {
      const box = shown.getBoundingClientRect();
      shown.style.left = `${Math.max(0, Math.min(box.left, window.innerWidth - box.width))}px`;
      shown.style.top = `${Math.max(0, Math.min(box.top, window.innerHeight - box.height))}px`;
    }
  });
  const dismissOnPointer = (event: PointerEvent): void => {
    if (!menuHost.contains(event.target as Node | null)) {
      closeMenu();
    }
  };
  const dismissOnEscape = (event: KeyboardEvent): void => {
    if (event.key === 'Escape') {
      closeMenu();
    }
  };
  document.addEventListener('pointerdown', dismissOnPointer, true);
  document.addEventListener('keydown', dismissOnEscape, true);

  const screen: TerminalScreen = {
    write: (data) => emulator.write(data),
    writeLine: (text, tone) => emulator.writeln(ownLine(text, tone)),
  };
  const session = new TerminalSession(peer, commands, screen, locale);
  const draw = (): void => {
    render(
      terminalChrome(session.status, locale, () => {
        // `onClose` runs even when the close failed. The call has been made
        // either way, the session's own end kills the shell again behind it,
        // and a button that does nothing because the host was unreachable is
        // the state this whole change exists to remove.
        void session
          .close()
          .catch(reportFailure('the shell could not be closed'))
          .then(onClose);
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
          // The size asked for at open is the size the window had then; one
          // that changed while the host was answering has no shell to reach
          // until now.
          if (session.status === 'open') {
            void session
              .resize(emulator.cols, emulator.rows)
              .catch(reportFailure('the shell could not be resized'));
          }
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
      resizer.disconnect();
      cancelAnimationFrame(fitting);
      document.removeEventListener('pointerdown', dismissOnPointer, true);
      document.removeEventListener('keydown', dismissOnEscape, true);
      menuHost.remove();
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
