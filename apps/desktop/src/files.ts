// File manager window entry point (ADR 0124).
//
// One window per session, labelled `files-{peer}` beside a screen session or
// `view-{peer}` when the file manager *is* the session — "File manager" on a
// remembered host, with no picture behind it. The peer label arrives as a
// query-string parameter set by the Rust side, exactly as the view window's
// does (§15).
//
// This file is only wiring: the state and every decision live in
// `file-manager.ts`, which the tests drive directly.

import { render } from 'lit-html';

import { FileManager, renderFileManager, tauriFileManagerCommands, type DropTarget, type Side } from './file-manager';
import { tauriFileCommands } from './file-transfers';
import { dirOf, resolveLocale, t, type Locale } from './i18n';
import { applyTheme } from './theme';

/** How often the window re-reads the session while something is moving. */
const BUSY_POLL_MS = 400;
/** How often it re-reads the session otherwise. */
const IDLE_POLL_MS = 1000;

const params = new URLSearchParams(window.location.search);
const peer = params.get('peer') ?? '';
/** The remembered host's stable label, when the window was given one. */
const host = params.get('host') ?? '';
/**
 * Whether this window is the whole session (ADR 0124): opened from "File
 * manager" on a remembered host, so closing it ends the session, the same way
 * closing a screen or a terminal window does.
 */
const standalone = params.get('standalone') === '1';
const root = document.querySelector<HTMLElement>('#files');
const locale: Locale = resolveLocale(navigator);

document.documentElement.lang = locale;
document.documentElement.dir = dirOf(locale);
document.title = `Lumepeer — ${t(locale, 'fileManager.heading')}`;
applyTheme();

// Where each pane was left, kept per viewer: a convenience, and only ever
// read back as a place to start — a folder that is gone falls back to home
// on this side and to the drive roots on the other.
const LOCAL_KEY = 'lumepeer.files.local';
const remoteKey = host ? `lumepeer.files.remote.${host}` : '';

function stored(key: string): string | null {
  if (!key) {
    return null;
  }
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function store(key: string, value: string): void {
  if (!key || !value) {
    return;
  }
  try {
    localStorage.setItem(key, value);
  } catch {
    // Storage can be unavailable; the next window simply starts at home.
  }
}

let scheduled = false;
function draw(): void {
  if (scheduled) {
    return;
  }
  scheduled = true;
  requestAnimationFrame(() => {
    scheduled = false;
    if (root) {
      render(renderFileManager(fm, locale, { standalone, onDisconnect: () => void endSession() }), root);
    }
    store(LOCAL_KEY, fm.local.path);
    store(remoteKey, fm.remote.path);
    focusEditor();
  });
}

const fm = new FileManager({
  peer,
  locale,
  commands: tauriFileManagerCommands(peer),
  fileCommands: tauriFileCommands,
  onChange: draw,
  localStart: stored(LOCAL_KEY),
  remoteStart: stored(remoteKey),
});

/** Puts the caret in a name field the moment one appears, text selected. */
let focusedEditor: HTMLInputElement | null = null;
function focusEditor(): void {
  const input = root?.querySelector<HTMLInputElement>('.fm-name-input') ?? null;
  if (input && input !== focusedEditor) {
    focusedEditor = input;
    input.focus();
    // A rename selects the name without its extension, the way desktop file
    // managers do, so typing replaces the part people usually change.
    const dot = input.value.lastIndexOf('.');
    input.setSelectionRange(0, dot > 0 ? dot : input.value.length);
  } else if (!input) {
    focusedEditor = null;
  }
  const dialog = root?.querySelector<HTMLButtonElement>('[data-testid="fm-confirm-delete"]');
  if (dialog && document.activeElement?.closest('.fm-dialog') === null) {
    dialog.focus();
  }
}

/** The pane — and folder row in it, if any — under a point in the window. */
function hitTest(x: number, y: number): DropTarget | null {
  const element = document.elementFromPoint(x, y);
  const pane = element?.closest<HTMLElement>('[data-pane]');
  const side = pane?.dataset.pane;
  if (side !== 'local' && side !== 'remote') {
    return null;
  }
  const row = element?.closest<HTMLElement>('[data-name]');
  const folder = row && row.dataset.dir === '1' ? (row.dataset.name ?? null) : null;
  return { side: side as Side, folder };
}

/// Ends the session this window is. Only ever for a standalone window.
async function endSession(): Promise<void> {
  const { invoke } = await import('@tauri-apps/api/core');
  await invoke('session_revoke', { args: { peer } });
}

async function main(): Promise<void> {
  draw();

  // Keys the window owns: the file manager's shortcuts, and the webview's own
  // reload keys, which would otherwise throw the window away mid-transfer.
  document.addEventListener('keydown', (event) => {
    if (fm.onKey(event)) {
      event.preventDefault();
    }
  });
  // The native menu offers "reload" and "inspect"; the panes have their own.
  document.addEventListener('contextmenu', (event) => {
    if (!(event.target as HTMLElement | null)?.closest('input')) {
      event.preventDefault();
    }
  });

  // Dragging between the panes, with pointer events: native drag and drop is
  // on for this window so files can come in from the desktop, and on Windows
  // that handler swallows the page's own HTML drag events.
  document.addEventListener('pointermove', (event) => {
    if (fm.drag) {
      fm.dragMove(event.clientX, event.clientY, hitTest(event.clientX, event.clientY));
    }
  });
  const endDrag = (): void => {
    if (fm.drag) {
      fm.dragEnd();
    }
  };
  document.addEventListener('pointerup', endDrag);
  document.addEventListener('pointercancel', endDrag);
  window.addEventListener('blur', endDrag);

  try {
    const { getCurrentWebview } = await import('@tauri-apps/api/webview');
    await getCurrentWebview().onDragDropEvent((event) => {
      const payload = event.payload;
      const ratio = window.devicePixelRatio || 1;
      if (payload.type === 'leave') {
        fm.desktopDragOver(null);
        return;
      }
      const at = hitTest(payload.position.x / ratio, payload.position.y / ratio);
      if (payload.type === 'drop') {
        void fm.dropFromDesktop(payload.paths, at);
      } else {
        fm.desktopDragOver(at?.side === 'remote' ? at : null);
      }
    });
  } catch (error) {
    // Outside Tauri (a test page, a plain browser) there is nothing to drop
    // from; the panes still work.
    console.warn('file drops from the desktop are not available:', error);
  }

  if (standalone) {
    try {
      const { getCurrentWindow } = await import('@tauri-apps/api/window');
      await getCurrentWindow().onCloseRequested(async () => {
        // Awaited and bounded, as the view window's is: Tauri destroys the
        // window as soon as this returns, and a revoke still on its way out
        // would leave the session up at both ends (ADR 0119).
        await Promise.race([
          endSession().catch(() => undefined),
          new Promise((resolve) => setTimeout(resolve, 2000)),
        ]);
      });
    } catch (error) {
      console.warn('the close of this window cannot end the session:', error);
    }
  }

  await fm.start();
  const loop = async (): Promise<void> => {
    await fm.poll();
    setTimeout(() => void loop(), fm.queue.pending() ? BUSY_POLL_MS : IDLE_POLL_MS);
  };
  void loop();
}

void main();
