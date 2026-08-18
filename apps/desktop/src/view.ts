// Remote-view window entry point (design doc §11, §13).
//
// One window per watched host, labelled `view-{peer}`; the peer label and the
// grant it opened with arrive as query-string parameters set by the Rust actor
// that created the window. The label is the pseudonym of §15, never a NodeId.
//
// This file is only wiring: everything with a decision in it lives in
// `view-window.ts`, which the tests drive directly.

import { render } from 'lit-html';

import { detectLocale, dirOf, t, type Locale } from './i18n';
import {
  decodeViewFrame,
  paintFrame,
  ViewInput,
  viewOverlay,
  type InputSink,
  type ViewStatus,
} from './view-window';

const params = new URLSearchParams(window.location.search);
const peer = params.get('peer') ?? '';
const canvas = document.querySelector<HTMLCanvasElement>('#screen');
const overlay = document.querySelector<HTMLElement>('#overlay');
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
    const response = await invoke('view_next_frame', { args: { peer } });
    const frame = decodeViewFrame(response as ArrayBuffer);
    if (frame.status !== status) {
      status = frame.status;
      renderOverlay();
    }
    // The grant is live: a host that lowered the role mid-session takes the
    // listeners away again on the very next frame (§8.1).
    input?.setEnabled(frame.input);
    paintFrame(canvas, frame);
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
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().onCloseRequested(() => {
    stopped = true;
    input?.setEnabled(false);
    void endSession();
  });
  loop();
}

void main();
