// Remote-view window entry point (design doc §11, §13).
//
// One window per watched host, labelled `view-{peer}`; the peer label and the
// grant it opened with arrive as query-string parameters set by the Rust actor
// that created the window. The label is the pseudonym of §15, never a NodeId.
//
// This file is only wiring: everything with a decision in it lives in
// `view-window.ts` and `view-hotkeys.ts`, which the tests drive directly.

import { render } from 'lit-html';

// The emulator's own stylesheet, imported statically so the bundler emits it
// as a file this page links: a dynamic CSS import would arrive as an inline
// `<style>`, which §13's `style-src 'self'` refuses (ADR 0079, ADR 0081).
import '@xterm/xterm/css/xterm.css';

import {
  browseDenied,
  emptyState,
  fileManagerPanel,
  refresh as refreshFileManager,
  tauriFileManagerCommands,
  type FileManagerState,
} from './file-manager';
import { tauriFileCommands, type FileTransfers } from './file-transfers';

import {
  ChatState,
  startChatPolling,
  tauriChatCommands,
  type ChatCommands,
  type ChatRow,
} from './chat';
import { detectLocale, dirOf, t, type Locale } from './i18n';
import { mountTerminal, type TerminalControls } from './terminal';
import { mountToolbar, tauriToolbarCommands, type ToolbarControls } from './toolbar';
import {
  decodeViewChunk,
  NativeDecoder,
  nativeDecodingAvailable,
  type ViewChunk,
} from './view-decoder';
import { installHotkeys } from './view-hotkeys';
import {
  clampPan,
  cursorCssFor,
  cursorPlacement,
  decodeCursorShape,
  decodeViewFrame,
  defaultLayout,
  displaySize,
  effectiveScale,
  FALLBACK_PICTURE_CEILING,
  frameResized,
  imageRenderingFor,
  installPan,
  NATIVE_PICTURE_CEILING,
  nextDisplayMode,
  paintCursor,
  paintFrame,
  pictureBox,
  recordingBadge,
  streamSizeFor,
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
const filePanel = document.querySelector<HTMLElement>('#file-panel');
const terminalPanel = document.querySelector<HTMLElement>('#terminal-panel');
const terminalChrome = document.querySelector<HTMLElement>('#terminal-chrome');
const terminalScreen = document.querySelector<HTMLElement>('#terminal-screen');
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
// The terminal, once somebody has opened it. Mounted on first use rather than
// with the window: this window's job is the picture, and a session that never
// asks for a shell should not pay for an emulator (ADR 0079).
let terminal: TerminalControls | null = null;
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
// The file manager's own two listings, polled while its panel exists (ADR
// 0076). Held here rather than inside the panel because the toolbar button
// has to know whether the host allows it at all, and that answer arrives on
// the same poll.
const fileState: FileManagerState = emptyState();
let fileTransfers: FileTransfers = { offers: [], transfers: [] };
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

/** How often the file manager re-reads both of its panes, in milliseconds. */
const FILE_MANAGER_POLL_MS = 1000;

const fileManagerCommands = tauriFileManagerCommands(peer);

/** Draws the file manager panel from whatever the last poll left behind. */
function renderFileManager(): void {
  if (!filePanel) {
    return;
  }
  // A withdrawn grant takes the panel with it, not just its contents (ADR
  // 0076): the toolbar button disappears on the same condition, so a guest
  // is never left pressing something that cannot work.
  if (browseDenied(fileState)) {
    filePanel.hidden = true;
    toolbar?.redraw();
    return;
  }
  render(
    fileManagerPanel(
      peer,
      fileState,
      fileTransfers,
      locale,
      fileManagerCommands,
      tauriFileCommands,
      renderFileManager,
    ),
    filePanel,
  );
}

/**
 * Mounts the terminal the first time the panel is opened, and asks the host
 * for a shell (ADR 0079).
 *
 * Nothing is granted by asking. The answer — an id or one of the four
 * refusals of §18 — arrives on the panel's own poll and is drawn there.
 */
async function openTerminal(): Promise<void> {
  if (!terminalScreen || !terminalChrome) {
    return;
  }
  try {
    terminal ??= await mountTerminal(terminalScreen, terminalChrome, locale, peer);
    await terminal.start();
  } catch (error) {
    console.error('the terminal could not be opened:', error);
  }
}

/** Re-reads both panes and the transfer list, then draws them. */
async function pollFileManager(): Promise<void> {
  try {
    await refreshFileManager(fileState, fileManagerCommands);
    fileTransfers = await tauriFileCommands.list();
  } catch {
    // The session ended, or the host is not answering; the window closes
    // through its own path and nothing here claims otherwise.
    return;
  }
  renderFileManager();
}

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
  // Every reason this runs - a resize, a zoom, a display-mode change, a frame
  // that arrived at a new size - is a reason the host's idea of the picture
  // size may now be wrong. `streamSizeFor` deliberately does not read
  // `frameSize`, so answering a resized frame here cannot feed back into
  // another request (ADR 0060).
  syncStreamSize();
}

/**
 * How long a window has to stop changing size before the host is told about
 * it (ADR 0060).
 *
 * A drag across the screen produces a `resize` per animation frame, and each
 * distinct size the host accepts costs an encoder type renegotiation and the
 * keyframe that follows it - the most expensive frame in the stream. Waiting
 * for the gesture to end turns a drag into one request. Short enough that
 * letting go of the window edge and seeing it sharpen reads as immediate.
 */
const STREAM_SIZE_SETTLE_MS = 250;

/**
 * The largest picture this window may ask for.
 *
 * Starts at the fallback ceiling and is raised once the `WebView` has proved
 * it decodes the bitstream itself: until that is known, the RGBA slot of
 * §11.3 is the path the picture might take, and that slot is what the
 * smaller ceiling measures (ADR 0018, ADR 0060).
 */
let pictureCeiling: { width: number; height: number } = FALLBACK_PICTURE_CEILING;
/** The size the host was last told about, so a repeat costs nothing. */
let sentStreamSize: { width: number; height: number } | null = null;
let streamSizeTimer: ReturnType<typeof setTimeout> | undefined;

/**
 * Tells the host the picture size this window is drawing, once the size has
 * settled (§11; ADR 0060).
 *
 * Debounced rather than rate-limited: what matters is the size the window
 * ends up at, and every value on the way there is one nobody will look at.
 */
function syncStreamSize(): void {
  const wanted = streamSizeFor(layout, viewportBox(), window.devicePixelRatio || 1, pictureCeiling);
  if (
    !wanted ||
    (sentStreamSize?.width === wanted.width && sentStreamSize.height === wanted.height)
  ) {
    return;
  }
  clearTimeout(streamSizeTimer);
  streamSizeTimer = setTimeout(() => {
    if (stopped) {
      return;
    }
    // Recomputed rather than captured: the window may have moved again while
    // this was waiting, and the point of waiting is to send where it landed.
    const now = streamSizeFor(layout, viewportBox(), window.devicePixelRatio || 1, pictureCeiling);
    if (!now || (sentStreamSize?.width === now.width && sentStreamSize.height === now.height)) {
      return;
    }
    sentStreamSize = now;
    void tauriToolbarCommands.viewSetSize(peer, now.width, now.height).catch(() => {
      // An older host that never advertised the feature, or a session that
      // just ended. Neither is worth a message: the picture keeps arriving
      // at whatever size that host already chose.
      sentStreamSize = null;
    });
  }, STREAM_SIZE_SETTLE_MS);
}

function setDisplayMode(mode: DisplayMode): void {
  // Switching *into* `scaled` had nothing to switch to: `scale` starts at 1
  // and only Ctrl+wheel ever moved it, so picking "Zoom" from the settings
  // list landed on exactly what "Actual size" does and the two entries were
  // indistinguishable. Seeding it with the scale already on screen makes the
  // switch continuous — from a fitted 0.5 the picture stays where it was —
  // and the zoom row in the popover is what moves it from there.
  const scale =
    mode === 'scaled'
      ? effectiveScale(layout, frameSize, viewportBox(), window.devicePixelRatio || 1)
      : layout.scale;
  // A mode change re-centres: the pan that made sense at one size is an
  // arbitrary offset at another.
  layout = { ...layout, mode, scale, offsetX: 0, offsetY: 0 };
  applyLayout();
  toolbar?.redraw();
}

/** One zoom notch, from wherever the picture is now (§11). */
function zoomView(steps: number): void {
  layout = zoomBy(layout, steps, frameSize, viewportBox(), window.devicePixelRatio || 1);
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
// Only the fallback path uses it; the bitstream path has no such round trip
// to save (ADR 0058).
let lastPaintedUs = 0;
// The window's own H.264 decoder, when this `WebView` has one. `null` is the
// fallback: the Rust side decodes into RGBA in the sandboxed worker of §11.3
// and this window polls for pixels, which is what it always did.
let nativeDecoder: NativeDecoder | null = null;
// Whether the next chunk request also asks the host for an intra frame. Set
// on the very first request — nothing can be decoded before one — and again
// whenever the decoder loses its footing.
let needKeyframe = true;

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
    applySessionFlags(frame);
    if (paintFrame(canvas, frame)) {
      lastPaintedUs = frame.timestampUs;
      // The remote screen can change resolution mid-session, and every part
      // of the layout is a function of the frame's size — but only of that,
      // of the window's size and of the display mode, so this is the only
      // per-frame reason to lay out again. It used to run on every painted
      // frame, thirty times a second, for an answer that changed perhaps
      // twice a session.
      if (frameResized(frame, frameSize)) {
        frameSize = { width: frame.width, height: frame.height };
        applyLayout();
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

/**
 * The three things every answer from the host carries, whichever path the
 * picture itself took: how the pipeline is doing, whether this session may
 * still send input, and whether the host says it is recording.
 */
function applySessionFlags(frame: { status: ViewStatus; input: boolean; recording: boolean }): void {
  if (frame.status !== status) {
    status = frame.status;
    renderOverlay();
  }
  // The grant is live: a host that lowered the role mid-session takes the
  // listeners away again on the very next answer (§8.1).
  input?.setEnabled(frame.input);
  // The host is the only one who knows, and it says so every time: a
  // recording that started a moment ago is on screen a moment later (§17).
  if (frame.recording !== recording) {
    recording = frame.recording;
    renderRecording();
  }
  // Nothing to point at while the input grant is withdrawn, so nothing to
  // draw a pointer for either.
  if (!frame.input && pointerAt !== null) {
    pointerAt = null;
    placeCursor();
  }
}

/**
 * One turn of the bitstream loop (ADR 0058).
 *
 * There is no pacing here on purpose. `view_next_chunk` does not answer until
 * a frame exists, so this loop is asleep exactly as long as the host has
 * nothing to show and wakes the moment it does — where the RGBA loop below
 * asks on every animation frame and pays a full round trip whether or not
 * anything changed. Everything that arrived since the last turn comes back in
 * one answer, in order, because an inter frame is meaningless without the
 * frames it references.
 */
async function nativeTick(): Promise<void> {
  if (stopped || !nativeDecoder) {
    return;
  }
  try {
    const invoke = await invoker();
    const response = await invoke('view_next_chunk', {
      args: { peer, need_keyframe: needKeyframe },
    });
    needKeyframe = false;
    const chunk: ViewChunk = decodeViewChunk(response as ArrayBuffer);
    applySessionFlags(chunk);
    if (chunk.desync) {
      // Frames were lost or the media connection was redialled: everything
      // the decoder holds refers to pictures it will never see.
      nativeDecoder.reset();
      needKeyframe = true;
    }
    if (nativeDecoder.push(chunk.frames, chunk.codec).needKeyframe) {
      needKeyframe = true;
    }
  } catch {
    // The view is gone (session ended, window closing).
    stopped = true;
    input?.setEnabled(false);
  }
}

function nativeLoop(): void {
  void nativeTick().finally(() => {
    if (!stopped) {
      // Straight back in, not through `requestAnimationFrame`: the wait is
      // already inside the call, and an animation frame would add a tick of
      // its own and stop the stream entirely whenever the window is hidden.
      nativeLoop();
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
      toggleFiles(): boolean {
        if (!filePanel) {
          return false;
        }
        filePanel.hidden = !filePanel.hidden;
        if (!filePanel.hidden) {
          renderFileManager();
        }
        toolbar?.redraw();
        return !filePanel.hidden;
      },
      filesVisible(): boolean {
        return filePanel !== null && !filePanel.hidden;
      },
      filesAvailable(): boolean {
        return !browseDenied(fileState);
      },
      toggleTerminal(): boolean {
        if (!terminalPanel) {
          return false;
        }
        terminalPanel.hidden = !terminalPanel.hidden;
        if (!terminalPanel.hidden) {
          void openTerminal();
        }
        toolbar?.redraw();
        return !terminalPanel.hidden;
      },
      terminalVisible(): boolean {
        return terminalPanel !== null && !terminalPanel.hidden;
      },
      displayMode: () => layout.mode,
      setDisplayMode,
      zoomPercent: () =>
        Math.round(
          effectiveScale(layout, frameSize, viewportBox(), window.devicePixelRatio || 1) * 100,
        ),
      zoomBy: zoomView,
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
        zoomView(event.deltaY < 0 ? 1 : -1);
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
  // The file manager polls whether or not its panel is open: the answer that
  // says whether the host allows it at all is the same one that fills the
  // panes, and the toolbar button is drawn from it (ADR 0076).
  if (filePanel) {
    filePanel.hidden = true;
    void pollFileManager();
    setInterval(() => {
      void pollFileManager();
    }, FILE_MANAGER_POLL_MS);
  }
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
    nativeDecoder?.close();
    // The shell goes with the window that asked for it. The host kills it at
    // its own end of the session too, so this is belt and braces on purpose:
    // a process left running on somebody else's machine is the one outcome
    // ADR 0079 may not produce.
    terminal?.stop();
    void endSession();
  });
  // Which side decodes. The window is the only one that knows whether its own
  // `WebView` has a usable `VideoDecoder`, and the Rust side reads the answer
  // off whichever command the first call uses — so asking here, once, is also
  // what tells it whether to start the worker process at all (ADR 0058).
  if (canvas && (await nativeDecodingAvailable())) {
    // This window decodes for itself, so the RGBA slot of §11.3 is not on the
    // path and its ceiling does not apply: the host may send its own screen
    // at its own size (ADR 0058, ADR 0060).
    pictureCeiling = NATIVE_PICTURE_CEILING;
    syncStreamSize();
    nativeDecoder = new NativeDecoder(canvas, (width, height) => {
      frameSize = { width, height };
      applyLayout();
    });
    nativeLoop();
  } else {
    loop();
  }
}

void main();
