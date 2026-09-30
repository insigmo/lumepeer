// A terminal window beside a session (ADR 0131).
//
// What "Connect to terminal" opens for a host this node already has a
// session with: the second connect would be refused, so the shell rides the
// session that is already there, in a window of its own. It is the terminal
// panel of the view window and nothing else — no picture, no toolbar, no
// chat — and closing it ends its shell, not the session.
//
// The Rust side knows this window by its label (`term-{peer}`), and that is
// what keeps its shells apart from the ones the view window's own panel
// opened: both poll the same host, and each is told only about its own.

import '@xterm/xterm/css/xterm.css';

import { detectLocale, dirOf } from './i18n';
import { mountTerminal, type TerminalControls } from './terminal';

const params = new URLSearchParams(window.location.search);
/** Pseudonymized label of the session this window's shells ride (§15). */
const peer = params.get('peer') ?? '';
const locale = detectLocale(navigator);

document.documentElement.lang = locale;
document.documentElement.dir = dirOf(locale);

let terminal: TerminalControls | null = null;

/** The window goes once its shell does: a shell is all it ever shows. */
function closeWindow(): void {
  void import('@tauri-apps/api/window')
    .then(({ getCurrentWindow }) => getCurrentWindow().close())
    .catch((error: unknown) => {
      console.error('the terminal window could not be closed:', error);
    });
}

async function main(): Promise<void> {
  const screen = document.querySelector<HTMLElement>('#terminal-screen');
  const chrome = document.querySelector<HTMLElement>('#terminal-chrome');
  if (!screen || !chrome) {
    return;
  }
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  // The shell goes with the window. The Rust side ends it too once the
  // window is destroyed, because Tauri may destroy it before this reaches
  // the actor; asking from here as well is what makes the ordinary close
  // prompt rather than eventual.
  await getCurrentWindow().onCloseRequested(() => {
    terminal?.stop();
  });
  terminal = await mountTerminal(screen, chrome, locale, peer, closeWindow);
  await terminal.start();
}

main().catch((error: unknown) => {
  console.error('the terminal could not be opened:', error);
});
