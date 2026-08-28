// Remote-view window entry point (design doc §11, §13).
//
// One window per watched host, labelled `view-{peer}`; the peer label and the
// grant it opened with arrive as query-string parameters set by the Rust actor
// that created the window. The label is the pseudonym of §15, never a NodeId.
//
// This file is only wiring: everything with a decision in it lives in
// `view-window.ts` and `view-hotkeys.ts`, which the tests drive directly.

import { render } from 'lit-html';

import {
  ChatState,
  startChatPolling,
  tauriChatCommands,
  type ChatCommands,
  type ChatRow,
} from './chat';
import { detectLocale, dirOf, t, type Locale } from './i18n';
import { mountToolbar, tauriToolbarCommands, type ToolbarControls } from './toolbar';
import { installHotkeys } from './view-hotkeys';
import {
  clampPan,
  cursorCssFor,
  cursorPlacement,
  decodeCursorShape,
  decodeViewFrame,
  defaultLayout,
  displaySize,
  imageRenderingFor,
  installPan,
  nextDisplayMode,
  paintCursor,
  paintFrame,
  pictureBox,
  recordingBadge,
  suppressContextMenu,
  ViewInput,
  viewOverlay,
  zoomBy,
  type Box,
  type CursorShape,
  type DisplayMode,
  type InputSink,
  type ViewLayout,
  type ViewStatus,
} from './view-window';

/**
 * How often the window asks whether the host's cursor changed.
 *
 * Not a frame rate: the *position* is local and instant, and only the bitmap
 * comes from the host. A quarter of a second is well under the time it takes
 * to notice a shape is wrong, and two orders of magnitude cheaper than asking
 * with every frame.
 */
const CURSOR_POLL_INTERVAL_MS = 250;

const params = new URLSearchParams(window.location.search);
const peer = params.get('peer') ?? '';
const canvas = document.querySelector<HTMLCanvasElement>('#screen');
const cursorLayer = document.querySelector<HTMLCanvasElement>('#cursor');
const surface = document.querySelector<HTMLElement>('#view');
const toolbarRootElement = document.querySelector<HTMLElement>('#toolbar-root');
const overlay = document.querySelector<HTMLElement>('#overlay');
const recordingIndicator = document.querySelector<HTMLElement>('#recording-indicator');
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

// How the picture is laid out, and the size of the last frame it was laid out
// for. Both live here rather than in `view-window.ts` because they are window
// state, not a decision: every rule about them is a pure function over there.
let layout: ViewLayout = defaultLayout();
let frameSize = { width: 0, height: 0 };
let fullscreen = false;
let toolbar: ToolbarControls | null = null;
// The host's cursor, once it has announced one. A host that still draws the
// cursor into the picture announces none, and this stays null — which is what
// keeps the overlay off rather than putting a second cursor on screen (§11).
let cursor: CursorShape | null = null;
// Whether the overlay is drawn at all. Only meaningful once a shape has
// arrived: without one there is nothing to turn off, and the toolbar says so
// by disabling the switch.
let localCursor = true;
// The chat panel opens closed (§11), so a message from the host would arrive
// with nothing on screen to show it. This is the whole of the indication: one
// flag, marked on the toolbar's chat button and cleared when the panel opens.
let chatUnread = false;
// Incoming rows already on screen. Compared against the transcript rather than
// counted from an event, because the actor's transcript is the only thing that
// knows what arrived.
let chatSeenIncoming = 0;
const chatState = new ChatState();
// The last pointer position inside the window, which is where the cursor is
// drawn. Local by design: a cursor that moved with the video would lag by the
// round trip, and removing that lag is the whole reason for the channel.
let pointerAt: { x: number; y: number } | null = null;

function incomingCount(rows: readonly ChatRow[]): number {
  return rows.reduce((total, row) => (row.outgoing ? total : total + 1), 0);
}

/** Clears the unread mark: everything in the transcript is now on screen. */
function markChatRead(): void {
  chatSeenIncoming = incomingCount(chatState.transcript);
  chatUnread = false;
}

/**
 * The transcript poll, wrapped rather than replaced.
 *
 * `startChatPolling` already re-reads the authoritative transcript once a
 * second and owns the panel's markup; the only thing missing is that nobody
 * notices a message that arrived while the panel was closed. Wrapping the
 * command it polls through is where that fits without the panel having to know
 * anything about the toolbar.
 */
const chatCommands: ChatCommands = {
  async chatTranscript(label) {
    const rows = await tauriChatCommands.chatTranscript(label);
    const incoming = incomingCount(rows);
    if (chatPanel && !chatPanel.hidden) {
      chatSeenIncoming = incoming;
    } else if (incoming > chatSeenIncoming) {
      chatSeenIncoming = incoming;
      if (!chatUnread) {
        chatUnread = true;
        toolbar?.redraw();
      }
    }
    return rows;
  },
  chatSend(label, text) {
    return tauriChatCommands.chatSend(label, text);
  },
};

function viewportBox(): Box {
  if (!surface) {
    return { left: 0, top: 0, width: 0, height: 0 };
  }
  const rect = surface.getBoundingClientRect();
  return { left: rect.left, top: rect.top, width: rect.width, height: rect.height };
}

/** Where the picture is on screen right now, for the pointer mapping. */
function currentPictureBox(): Box {
  return pictureBox(layout, frameSize, viewportBox(), window.devicePixelRatio || 1);
}

/** Whether this session carries the host's cursor on its own channel. */
function cursorChannelLive(): boolean {
  return cursor !== null;
}

/**
 * Moves the cursor layer to the pointer, or hides it, and keeps the canvas's
 * own `cursor` in step with it.
 *
 * Hidden whenever there is nothing honest to draw: no shape from the host, the
 * operator turned the overlay off, or the pointer is not over the picture.
 */
function placeCursor(): void {
  const visible = cursor !== null && localCursor && pointerAt !== null;
  if (canvas) {
    // The shape on the layer *is* the pointer while it is drawn, so the
    // system one underneath has to go — and has to come back the moment it
    // stops being drawn, or the toolbar becomes unusable.
    canvas.style.cursor = cursorCssFor(cursor !== null, localCursor, pointerAt !== null);
  }
  if (!cursorLayer) {
    return;
  }
  cursorLayer.hidden = !visible;
  if (!visible || !cursor || !pointerAt) {
    return;
  }
  const box = cursorPlacement(pointerAt, cursor, currentPictureBox(), frameSize);
  const viewport = viewportBox();
  cursorLayer.style.left = `${box.left - viewport.left}px`;
  cursorLayer.style.top = `${box.top - viewport.top}px`;
  cursorLayer.style.width = `${box.width}px`;
  cursorLayer.style.height = `${box.height}px`;
}

/**
 * Asks the actor whether the host has announced a different cursor.
 *
 * Polled on its own interval rather than with every frame: a cursor changes
 * when a pointer crosses a text field, not thirty times a second, and the
 * sequence number means an unchanged one costs a header and nothing else.
 */
async function pollCursor(): Promise<void> {
  if (stopped || !cursorLayer) {
    return;
  }
  try {
    const invoke = await invoker();
    const response = await invoke('view_cursor', {
      args: { peer, since_seq: cursor?.seq ?? 0 },
    });
    const next = decodeCursorShape(response as ArrayBuffer);
    // A sequence of 0 is a host that announces no cursor: it is still drawing
    // one into the picture, and drawing a second here would be worse than the
    // latency this channel exists to remove.
    if (next.seq === 0) {
      cursor = null;
      placeCursor();
      toolbar?.redraw();
      return;
    }
    if (next.seq !== cursor?.seq && paintCursor(cursorLayer, next)) {
      const arrived = cursor === null;
      cursor = next;
      if (arrived) {
        toolbar?.redraw();
      }
    }
    placeCursor();
  } catch {
    // The view is gone, or this host has no cursor channel: nothing to draw
    // and nothing to say about it.
  }
}

/**
 * Pushes the layout onto the canvas element.
 *
 * The element's CSS size and a translate, never `canvas.width`/`canvas.height`:
 * the backing buffer stays at the frame's own resolution so `putImageData`,
 * which cannot scale, keeps working unchanged.
 */
function applyLayout(): void {
  if (!canvas) {
    return;
  }
  const viewport = viewportBox();
  const ratio = window.devicePixelRatio || 1;
  layout = clampPan(layout, frameSize, viewport, ratio);
  const size = displaySize(layout, frameSize, viewport, ratio);
  canvas.style.width = `${size.width}px`;
  canvas.style.height = `${size.height}px`;
  canvas.style.maxWidth = 'none';
  canvas.style.maxHeight = 'none';
  canvas.style.transform = `translate(${layout.offsetX}px, ${layout.offsetY}px)`;
  // Only CSS knows how the picture is resampled, and only this knows the
  // scale it is being drawn at — the stylesheet's `pixelated` is right at 1:1
  // and above and wrong for every fitted window below it.
  canvas.style.imageRendering = imageRenderingFor(layout, frameSize, viewport, ratio);
  placeCursor();
}

function setDisplayMode(mode: DisplayMode): void {
  // A mode change re-centres: the pan that made sense at one size is an
  // arbitrary offset at another.
  layout = { ...layout, mode, offsetX: 0, offsetY: 0 };
  applyLayout();
  toolbar?.redraw();
}

function resetView(): void {
  layout = defaultLayout();
  applyLayout();
  placeCursor();
  toolbar?.redraw();
}

async function setFullscreen(on: boolean): Promise<void> {
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  await getCurrentWindow().setFullscreen(on);
  fullscreen = on;
  // The toolbar is the only way back out with a pointer, so in full screen it
  // hides itself and comes back on hover or focus rather than disappearing.
  toolbarRootElement?.classList.toggle('is-autohide', on);
  applyLayout();
  toolbar?.redraw();
}

const input = canvas ? new ViewInput(canvas, sink, document, currentPictureBox) : undefined;
let status: ViewStatus = 'waiting';
// Whether the host says it is recording. Repainted only on a change, like the
// overlay: the badge is on every frame, the DOM work is not.
let recording = false;
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

function renderRecording(): void {
  if (!recordingIndicator) {
    return;
  }
  render(recordingBadge(recording, locale), recordingIndicator);
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
    // The host is the only one who knows, and it says so on every frame: a
    // recording that started a moment ago is on screen a moment later (§17).
    if (frame.recording !== recording) {
      recording = frame.recording;
      renderRecording();
    }
    if (paintFrame(canvas, frame)) {
      lastPaintedUs = frame.timestampUs;
      // The remote screen can change resolution mid-session, and every part
      // of the layout is a function of the frame's size.
      if (frame.width !== frameSize.width || frame.height !== frameSize.height) {
        frameSize = { width: frame.width, height: frame.height };
      }
      applyLayout();
      // Nothing to point at while the input grant is withdrawn, so nothing to
      // draw a pointer for either.
      if (!frame.input && pointerAt !== null) {
        pointerAt = null;
        placeCursor();
      }
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
  renderRecording();
  // The chat panel polls the actor's transcript; it exists only while this
  // window does, so no explicit teardown beyond the poll's own stop. The
  // toolbar's chat button toggles exactly this panel.
  if (chatPanel) {
    // Closed on arrival: the picture is what this window is for, and the panel
    // sits on top of it. The poll still runs, so opening the panel shows the
    // history that is already there rather than a blank second of waiting.
    chatPanel.hidden = true;
    startChatPolling(chatPanel, chatState, locale, peer, chatCommands);
  }
  // The floating session toolbar (§11): drag handle, settings, monitor
  // picker, chat toggle, microphone, Ctrl+Alt+Del, full screen, collapse. It
  // stops with the window; nothing here outlives the session.
  if (toolbarRootElement && chatPanel) {
    mountToolbar(toolbarRootElement, locale, peer, tauriToolbarCommands, {
      toggleChat(): boolean {
        chatPanel.hidden = !chatPanel.hidden;
        if (!chatPanel.hidden) {
          markChatRead();
        }
        // The button's own `is-active`, `aria-pressed` and unread mark are all
        // read off this, and none of them updates itself.
        toolbar?.redraw();
        return !chatPanel.hidden;
      },
      chatVisible(): boolean {
        return !chatPanel.hidden;
      },
      chatUnread(): boolean {
        return chatUnread;
      },
      displayMode: () => layout.mode,
      setDisplayMode,
      cursorChannel: cursorChannelLive,
      localCursor: () => localCursor,
      toggleLocalCursor(): void {
        localCursor = !localCursor;
        placeCursor();
      },
      fullscreen: () => fullscreen,
      toggleFullscreen(): void {
        void setFullscreen(!fullscreen);
      },
      bind(controls): void {
        toolbar = controls;
      },
    });
  }
  // Panning and zooming are local: neither reaches the host, and both are
  // arranged so the plain left button — which does — is never taken.
  if (surface) {
    installPan(
      surface,
      {
        layout: () => layout,
        panBy(dx, dy): void {
          layout = { ...layout, offsetX: layout.offsetX + dx, offsetY: layout.offsetY + dy };
          applyLayout();
        },
      },
      document,
    );
    surface.addEventListener(
      'wheel',
      (event) => {
        if (!event.ctrlKey) {
          return;
        }
        event.preventDefault();
        layout = zoomBy(
          layout,
          event.deltaY < 0 ? 1 : -1,
          frameSize,
          viewportBox(),
          window.devicePixelRatio || 1,
        );
        applyLayout();
        toolbar?.redraw();
      },
      { passive: false },
    );
  }
  window.addEventListener('resize', applyLayout);
  // Where the local cursor is drawn. Tracked on the surface rather than on the
  // canvas so the pointer leaving the picture hides it instead of freezing it
  // at the edge.
  if (surface) {
    surface.addEventListener('pointermove', (event) => {
      pointerAt = { x: event.clientX, y: event.clientY };
      placeCursor();
    });
    surface.addEventListener('pointerleave', () => {
      pointerAt = null;
      placeCursor();
    });
  }
  setInterval(() => {
    void pollCursor();
  }, CURSOR_POLL_INTERVAL_MS);
  // Installed before the input forwarder attaches, and in the capture phase,
  // so a matched chord is marked before it can be sent to the host (§11).
  installHotkeys(document, {
    'toggle-fullscreen': () => void setFullscreen(!fullscreen),
    'cycle-display-mode': () => setDisplayMode(nextDisplayMode(layout.mode)),
    'reset-view': resetView,
    'toggle-chat': () => {
      if (chatPanel) {
        chatPanel.hidden = !chatPanel.hidden;
        if (!chatPanel.hidden) {
          markChatRead();
        }
        toolbar?.redraw();
      }
    },
    'send-cad': () => {
      void tauriToolbarCommands.sasRequest(peer).catch(() => {
        // Refused by the host or the session ended; the log has it, and
        // nothing here claims the sequence was delivered.
      });
    },
    'toggle-toolbar': () => toolbar?.toggleCollapsed(),
  });
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
