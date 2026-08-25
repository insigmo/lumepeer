// Remote-view window entry point (design doc §11, §13).
//
// One window per watched host, labelled `view-{peer}`; the peer label and the
// grant it opened with arrive as query-string parameters set by the Rust actor
// that created the window. The label is the pseudonym of §15, never a NodeId.
//
// This file is only wiring: everything with a decision in it lives in
// `view-window.ts`, which the tests drive directly.

import { render } from 'lit-html';

import { ChatState, startChatPolling, tauriChatCommands } from './chat';
import { detectLocale, dirOf, t, type Locale } from './i18n';
import { mountToolbar, tauriToolbarCommands } from './toolbar';
import {
  decodeViewFrame,
  paintFrame,
  suppressContextMenu,
  ViewInput,
  viewOverlay,
  type InputSink,
  type ViewStatus,
} from './view-window';

const params = new URLSearchParams(window.location.search);
const peer = params.get('peer') ?? '';
const canvas = document.querySelector<HTMLCanvasElement>('#screen');
const overlay = document.querySelector<HTMLElement>('#overlay');
const chatPanel = document.querySelector<HTMLElement>('#chat-panel');
const locale: Locale = detectLocale(navigator);

document.documentElement.lang = locale;
document.documentElement.dir = dirOf(locale);
canvas?.setAttribute('role', 'img');
canvas?.setAttribute('aria-label', t(locale, 'view.canvasLabel'));

async function invoker(): Promise<(cmd: string, args?: unknown) => Promise<unknown>> {
  const { invoke } = await import('@tauri-apps/api/core');
  return invoke as (cmd: string, args?: unknown) => Promise<unknown>;
}

/// Ends the session. Closing this window and revoking are one switch, not two.
async function endSession(): Promise<void> {
  const invoke = await invoker();
  await invoke('session_revoke', { args: { peer } });
}

const sink: InputSink = {
  pointerMove(x, y, modifiers) {
    void invoker().then((invoke) => invoke('input_pointer_move', { args: { peer, x, y, modifiers } }));
  },
  press(logical, scancode, modifiers, pressed) {
    void invoker().then((invoke) =>
      invoke('input_press', { args: { peer, logical, scancode, modifiers, pressed } }),
    );
  },
  wheel(dx, dy, modifiers) {
    void invoker().then((invoke) => invoke('input_wheel', { args: { peer, dx, dy, modifiers } }));
  },
};

const input = canvas ? new ViewInput(canvas, sink) : undefined;
let status: ViewStatus = 'waiting';
let stopped = false;
// Timestamp of the picture already painted, or 0 for none — sent back as
// `since_us` so the actor can skip re-serializing pixels this window
// already has when it's polled faster than the video actually updates.
let lastPaintedUs = 0;

function renderOverlay(): void {
  if (!overlay) {
    return;
  }
  render(
    viewOverlay(status, locale, () => {
      void closeWindow();
    }),
    overlay,
  );
}

async function closeWindow(): Promise<void> {
  stopped = true;
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().close();
}

async function tick(): Promise<void> {
  if (stopped || !canvas) {
    return;
  }
  try {
    const invoke = await invoker();
    const response = await invoke('view_next_frame', { args: { peer, since_us: lastPaintedUs } });
    const frame = decodeViewFrame(response as ArrayBuffer);
    if (frame.status !== status) {
      status = frame.status;
      renderOverlay();
    }
    // The grant is live: a host that lowered the role mid-session takes the
    // listeners away again on the very next frame (§8.1).
    input?.setEnabled(frame.input);
    if (paintFrame(canvas, frame)) {
      lastPaintedUs = frame.timestampUs;
    }
  } catch {
    // The view is gone (session ended, window closing): stop polling rather
    // than spinning on a command that will keep failing.
    stopped = true;
    input?.setEnabled(false);
  }
}

function loop(): void {
  void tick().finally(() => {
    if (!stopped) {
      requestAnimationFrame(loop);
    }
  });
}

async function main(): Promise<void> {
  renderOverlay();
  // The chat panel polls the actor's transcript; it exists only while this
  // window does, so no explicit teardown beyond the poll's own stop. The
  // toolbar's chat button toggles exactly this panel.
  if (chatPanel) {
    chatPanel.hidden = false;
    startChatPolling(chatPanel, new ChatState(), locale, peer, tauriChatCommands);
  }
  // The floating session toolbar (§11): drag handle, settings, monitor
  // picker, chat toggle, microphone, Ctrl+Alt+Del, collapse. It stops with
  // the window; nothing here outlives the session.
  const toolbarRoot = document.querySelector<HTMLElement>('#toolbar-root');
  if (toolbarRoot && chatPanel) {
    mountToolbar(toolbarRoot, locale, peer, tauriToolbarCommands, {
      toggleChat(): boolean {
        chatPanel.hidden = !chatPanel.hidden;
        return !chatPanel.hidden;
      },
      chatVisible(): boolean {
        return !chatPanel.hidden;
      },
    });
  }
  // The remote host's own right-click menu is part of the picture; the
  // local WebView's native one has no business appearing on top of it.
  suppressContextMenu(document);
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().onCloseRequested(() => {
    stopped = true;
    input?.setEnabled(false);
    void endSession();
  });
  loop();
}

void main();
