// Remote-view window logic (design doc §11, §13).
//
// Split from the `view.ts` entry point so every piece here is testable in
// jsdom: the decode of the binary IPC frame, the canvas paint, the overlay
// markup, and — the part that actually matters for §2.2 — the rule that
// pointer and keyboard listeners exist only while the session's `input` grant
// is live.
//
// Nothing here decides anything. `input` arrives on every frame from the Rust
// core and the host re-checks each event it receives; this module only stops
// sending events the guest already knows are not permitted.

import { html, type TemplateResult } from 'lit-html';

import type { Locale } from './i18n';
import { t } from './i18n';

/// Pipeline health as reported in the first byte of a `view_next_frame`
/// response.
///
/// `no-capture` and `no-encoder` are the host saying it cannot produce a
/// picture at all — a different thing from `failed`, which is a connection
/// that was lost. Nothing was lost in those two, and nothing is worth
/// retrying, so the window says so instead of blaming the network.
export type ViewStatus =
  | 'waiting'
  | 'live'
  | 'reconnecting'
  | 'failed'
  | 'no-capture'
  | 'no-encoder';

const STATUS_BY_CODE: readonly ViewStatus[] = [
  'waiting',
  'live',
  'reconnecting',
  'failed',
  'no-capture',
  'no-encoder',
];

/** Bytes of the fixed header every `view_next_frame` response carries. */
export const VIEW_RESPONSE_HEADER_BYTES = 18;

/** Flags byte bit: the session's `input` grant is live right now. */
export const VIEW_FLAG_INPUT = 0b01;
/** Flags byte bit: the host says it is recording this session (§17). */
export const VIEW_FLAG_RECORDING = 0b10;

/** Logical identifiers at or above this value are pointer buttons, not keys. */
export const POINTER_BUTTON_LOGICAL_BASE = 0xf0000000;

/** Normalized pointer coordinate space of the protocol: 0..=65535. */
const POINTER_RANGE = 65535;

/**
 * How the remote picture is laid out inside the window (§11).
 *
 * - `fit` scales the picture to the window, preserving its aspect ratio.
 * - `actual` is one frame pixel to one *device* pixel, which is what makes
 *   "1:1" mean the same thing on a HiDPI screen as on any other.
 * - `scaled` is whatever the operator asked for, with the same device-pixel
 *   unit as `actual`.
 */
export type DisplayMode = 'fit' | 'actual' | 'scaled';

/** The display modes in the order the cycle hotkey walks them. */
export const DISPLAY_MODES: readonly DisplayMode[] = ['fit', 'actual', 'scaled'];

/** Narrowest and widest zoom the operator may ask for. */
export const MIN_SCALE = 0.25;
export const MAX_SCALE = 4;

/** How much one wheel notch or one zoom press moves the scale. */
export const SCALE_STEP = 0.25;

/**
 * Scale at or above which the picture is blitted without smoothing.
 *
 * One frame pixel to one device pixel, or bigger. At exactly one there is
 * nothing to interpolate and smoothing only costs time; above it, nearest
 * neighbour is what keeps a magnified screenshot crisp instead of turning
 * text into a smear.
 *
 * Below it the opposite holds and it is not a close call: minifying with
 * nearest neighbour *drops* pixels, so the thin strokes of a glyph fall out
 * whole and the picture comes apart. The default mode is `fit`, which is
 * almost always below one — a 1920×1080 host in a 960×640 window is 0.5 —
 * so this threshold is the difference between readable remote text and not.
 */
export const PIXELATED_MIN_SCALE = 1;

/** Everything about where the picture is and how big it is drawn. */
export interface ViewLayout {
  mode: DisplayMode;
  /**
   * Device pixels per frame pixel, used by `scaled`.
   *
   * Deliberately *not* CSS pixels: at `1` on a 2x screen the picture would be
   * drawn at half its real resolution and "1:1" would be a lie.
   */
  scale: number;
  /** Pan, in CSS pixels, away from the centred position. */
  offsetX: number;
  offsetY: number;
}

/** A rectangle in the same coordinates `MouseEvent.clientX/Y` use. */
export interface Box {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** The layout a freshly opened window starts in. */
export function defaultLayout(): ViewLayout {
  return { mode: 'fit', scale: 1, offsetX: 0, offsetY: 0 };
}

/**
 * Device pixels per frame pixel for this layout.
 *
 * `fit` is computed from the window; the other two are the operator's own
 * number, and `actual` is the special case of it being exactly one.
 */
export function effectiveScale(
  layout: ViewLayout,
  frame: { width: number; height: number },
  viewport: { width: number; height: number },
  devicePixelRatio: number,
): number {
  if (frame.width <= 0 || frame.height <= 0) {
    return 1;
  }
  if (layout.mode === 'actual') {
    return 1;
  }
  if (layout.mode === 'scaled') {
    return clamp(layout.scale, MIN_SCALE, MAX_SCALE);
  }
  const scale = Math.min(
    (viewport.width * devicePixelRatio) / frame.width,
    (viewport.height * devicePixelRatio) / frame.height,
  );
  return scale > 0 && Number.isFinite(scale) ? scale : 1;
}

/**
 * How the canvas should be resampled at this layout: `pixelated` at
 * {@link PIXELATED_MIN_SCALE} and above, `auto` below it.
 *
 * Not a CSS rule, because CSS cannot see the scale: the picture's size comes
 * from the frame, the window and the display mode, all of which change at
 * runtime. The stylesheet's `pixelated` is the starting value, for the moment
 * before the first frame arrives, and this overrides it from then on.
 */
export function imageRenderingFor(
  layout: ViewLayout,
  frame: { width: number; height: number },
  viewport: { width: number; height: number },
  devicePixelRatio: number,
): 'pixelated' | 'auto' {
  const scale = effectiveScale(layout, frame, viewport, devicePixelRatio);
  return scale >= PIXELATED_MIN_SCALE ? 'pixelated' : 'auto';
}

/**
 * Whether a newly painted frame makes the layout stale.
 *
 * The layout is a function of the frame's size, the window's size and the
 * display mode, and of nothing that changes from one frame to the next — so
 * laying out on every painted frame spent a `getBoundingClientRect` and a full
 * recompute thirty times a second on an answer that was the same every time.
 * The window's own size is covered by the `resize` listener, and a mode or
 * zoom change lays out where it happens; this is the third and last thing that
 * can move the picture, and it moves rarely.
 */
export function frameResized(
  frame: { width: number; height: number },
  previous: { width: number; height: number },
): boolean {
  return frame.width !== previous.width || frame.height !== previous.height;
}

/**
 * The CSS size the canvas *element* must be given.
 *
 * The element's size, never `canvas.width`/`canvas.height`: the backing buffer
 * has to stay at the frame's own resolution, or `putImageData` — which cannot
 * scale — would have to be replaced by a per-frame manual resample.
 */
export function displaySize(
  layout: ViewLayout,
  frame: { width: number; height: number },
  viewport: { width: number; height: number },
  devicePixelRatio: number,
): { width: number; height: number } {
  const scale = effectiveScale(layout, frame, viewport, devicePixelRatio);
  return {
    width: (frame.width * scale) / devicePixelRatio,
    height: (frame.height * scale) / devicePixelRatio,
  };
}

/**
 * Where the picture actually sits on screen: centred in the viewport, then
 * moved by the pan.
 *
 * This is the geometry {@link remotePointer} needs, and computing it here
 * rather than reading it back off the DOM is what makes every display mode
 * testable in jsdom, which has no layout at all.
 */
export function pictureBox(
  layout: ViewLayout,
  frame: { width: number; height: number },
  viewport: Box,
  devicePixelRatio: number,
): Box {
  const { width, height } = displaySize(layout, frame, viewport, devicePixelRatio);
  return {
    left: viewport.left + (viewport.width - width) / 2 + layout.offsetX,
    top: viewport.top + (viewport.height - height) / 2 + layout.offsetY,
    width,
    height,
  };
}

/**
 * Maps a pointer position in window coordinates onto the protocol's
 * normalized remote coordinate space.
 *
 * The one place the conversion happens, and the reason it is a function of an
 * explicit box rather than of the canvas: the moment the picture can be zoomed
 * and panned, "the canvas fills the window" stops being true, and a mapping
 * that assumed it would send the host coordinates that are wrong by exactly
 * the pan — silently, with no error anywhere.
 *
 * `null` for a point outside the picture: there is no remote pixel under it,
 * and clamping to the edge would report a click the operator did not make.
 */
export function remotePointer(
  clientX: number,
  clientY: number,
  picture: Box,
): { x: number; y: number } | null {
  if (picture.width <= 0 || picture.height <= 0) {
    return null;
  }
  const x = ((clientX - picture.left) / picture.width) * POINTER_RANGE;
  const y = ((clientY - picture.top) / picture.height) * POINTER_RANGE;
  if (x < 0 || y < 0 || x > POINTER_RANGE || y > POINTER_RANGE) {
    return null;
  }
  return { x: clampCoordinate(x), y: clampCoordinate(y) };
}

/**
 * The pan needed to keep the picture reachable: never so far that none of it
 * is on screen.
 *
 * A picture smaller than the window cannot be panned at all — there is
 * nothing off-screen to bring into view, and letting it drift would only lose
 * it.
 */
export function clampPan(
  layout: ViewLayout,
  frame: { width: number; height: number },
  viewport: { width: number; height: number },
  devicePixelRatio: number,
): ViewLayout {
  const { width, height } = displaySize(layout, frame, viewport, devicePixelRatio);
  const limitX = Math.max(0, (width - viewport.width) / 2);
  const limitY = Math.max(0, (height - viewport.height) / 2);
  return {
    ...layout,
    offsetX: clamp(layout.offsetX, -limitX, limitX),
    offsetY: clamp(layout.offsetY, -limitY, limitY),
  };
}

/**
 * Applies a zoom step, switching the layout into `scaled` and keeping the
 * scale the operator was already looking at as the starting point.
 *
 * Zooming out of `fit` from `fit`'s own scale is what makes the first press
 * feel like a nudge rather than a jump.
 */
export function zoomBy(
  layout: ViewLayout,
  steps: number,
  frame: { width: number; height: number },
  viewport: { width: number; height: number },
  devicePixelRatio: number,
): ViewLayout {
  const from = effectiveScale(layout, frame, viewport, devicePixelRatio);
  return {
    ...layout,
    mode: 'scaled',
    scale: clamp(from + steps * SCALE_STEP, MIN_SCALE, MAX_SCALE),
  };
}

/** The next mode in {@link DISPLAY_MODES}, wrapping round. */
export function nextDisplayMode(mode: DisplayMode): DisplayMode {
  const index = DISPLAY_MODES.indexOf(mode);
  return DISPLAY_MODES[(index + 1) % DISPLAY_MODES.length] ?? 'fit';
}

function clamp(value: number, low: number, high: number): number {
  return Math.max(low, Math.min(high, value));
}

export interface ViewFrame {
  status: ViewStatus;
  /** Whether the session's `input` grant is live right now. */
  input: boolean;
  /**
   * Whether the host says it is recording this session (§17).
   *
   * The host's own statement, arriving on every frame like `input` does.
   * Nothing on this side infers it, and nothing on this side can turn the
   * indicator off while the host still says otherwise — "no hidden capture"
   * is the rule the badge exists for (§2.2).
   */
  recording: boolean;
  width: number;
  height: number;
  timestampUs: number;
  /** RGBA8 pixels; empty until the first picture is decoded. */
  pixels: Uint8ClampedArray;
}

/**
 * Parses the binary response of `view_next_frame`.
 *
 * Layout, little endian:
 * `status:u8 | flags:u8 | width:u32 | height:u32 | timestamp_us:u64 | RGBA8`.
 */
export function decodeViewFrame(buffer: ArrayBuffer): ViewFrame {
  if (buffer.byteLength < VIEW_RESPONSE_HEADER_BYTES) {
    throw new Error(`view frame is ${buffer.byteLength} bytes, expected at least ${VIEW_RESPONSE_HEADER_BYTES}`);
  }
  const header = new DataView(buffer, 0, VIEW_RESPONSE_HEADER_BYTES);
  const status = STATUS_BY_CODE[header.getUint8(0)];
  if (!status) {
    throw new Error(`unknown view status ${header.getUint8(0)}`);
  }
  const flags = header.getUint8(1);
  return {
    status,
    input: (flags & VIEW_FLAG_INPUT) !== 0,
    recording: (flags & VIEW_FLAG_RECORDING) !== 0,
    width: header.getUint32(2, true),
    height: header.getUint32(6, true),
    // The timestamp only ever orders frames; Number is exact well past any
    // realistic session length in microseconds.
    timestampUs: Number(header.getBigUint64(10, true)),
    pixels: new Uint8ClampedArray(buffer, VIEW_RESPONSE_HEADER_BYTES),
  };
}

/**
 * Paints a decoded picture onto the canvas, resizing it when the remote screen
 * changes resolution. Returns whether anything was painted.
 */
export function paintFrame(canvas: HTMLCanvasElement, frame: ViewFrame): boolean {
  if (frame.width === 0 || frame.height === 0 || frame.pixels.length < frame.width * frame.height * 4) {
    return false;
  }
  const context = canvas.getContext('2d');
  if (!context) {
    return false;
  }
  if (canvas.width !== frame.width || canvas.height !== frame.height) {
    canvas.width = frame.width;
    canvas.height = frame.height;
  }
  // `pixels` is a view onto the IPC buffer and may be longer than one picture
  // if the host ever pads; ImageData needs exactly width*height*4. The buffer
  // always comes from an IPC response, i.e. a plain (never shared) ArrayBuffer,
  // which is the only thing the cast asserts — no copy per frame.
  const exact = new Uint8ClampedArray(
    frame.pixels.buffer as ArrayBuffer,
    frame.pixels.byteOffset,
    frame.width * frame.height * 4,
  );
  context.putImageData(new ImageData(exact, frame.width, frame.height), 0, 0);
  return true;
}

/** Bytes of the fixed header every `view_cursor` response carries. */
export const CURSOR_RESPONSE_HEADER_BYTES = 12;

/**
 * The host's cursor, as the `view_cursor` IPC call describes it.
 *
 * `seq` is 0 when the host has announced no cursor at all — it is still
 * drawing one into the picture — and the overlay must stay empty, because two
 * cursors are worse than one that lags (§11).
 */
export interface CursorShape {
  seq: number;
  width: number;
  height: number;
  hotspotX: number;
  hotspotY: number;
  /** Premultiplied BGRA, or empty when this poll carried no new pixels. */
  pixels: Uint8ClampedArray;
}

/**
 * Parses the binary response of `view_cursor`.
 *
 * Layout, little endian:
 * `seq:u32 | width:u16 | height:u16 | hotspot_x:u16 | hotspot_y:u16 | BGRA8`.
 */
export function decodeCursorShape(buffer: ArrayBuffer): CursorShape {
  if (buffer.byteLength < CURSOR_RESPONSE_HEADER_BYTES) {
    throw new Error(
      `cursor response is ${buffer.byteLength} bytes, expected at least ${CURSOR_RESPONSE_HEADER_BYTES}`,
    );
  }
  const header = new DataView(buffer, 0, CURSOR_RESPONSE_HEADER_BYTES);
  return {
    seq: header.getUint32(0, true),
    width: header.getUint16(4, true),
    height: header.getUint16(6, true),
    hotspotX: header.getUint16(8, true),
    hotspotY: header.getUint16(10, true),
    pixels: new Uint8ClampedArray(buffer, CURSOR_RESPONSE_HEADER_BYTES),
  };
}

/**
 * Paints the host's cursor onto its own layer, at its own size.
 *
 * A separate canvas rather than the video one: drawn into the same context it
 * would have to be erased by hand on every frame, and the picture underneath
 * repaints thirty times a second while the cursor changes when someone crosses
 * a text field.
 *
 * The bytes arrive as premultiplied BGRA (see `CursorShapeData::rgba`); the
 * canvas wants straight RGBA, so the channels are swapped and the colour is
 * divided back out by the alpha. Skipping the un-premultiply leaves every
 * antialiased edge too dark against a light background.
 *
 * Returns whether anything was painted.
 */
export function paintCursor(canvas: HTMLCanvasElement, cursor: CursorShape): boolean {
  const pixelCount = cursor.width * cursor.height;
  if (pixelCount === 0 || cursor.pixels.length < pixelCount * 4) {
    return false;
  }
  const context = canvas.getContext('2d');
  if (!context) {
    return false;
  }
  if (canvas.width !== cursor.width || canvas.height !== cursor.height) {
    canvas.width = cursor.width;
    canvas.height = cursor.height;
  }
  const rgba = new Uint8ClampedArray(pixelCount * 4);
  for (let index = 0; index < pixelCount; index += 1) {
    const at = index * 4;
    const alpha = cursor.pixels[at + 3] ?? 0;
    const scale = alpha === 0 ? 0 : 255 / alpha;
    rgba[at] = Math.min(255, (cursor.pixels[at + 2] ?? 0) * scale);
    rgba[at + 1] = Math.min(255, (cursor.pixels[at + 1] ?? 0) * scale);
    rgba[at + 2] = Math.min(255, (cursor.pixels[at] ?? 0) * scale);
    rgba[at + 3] = alpha;
  }
  context.clearRect(0, 0, canvas.width, canvas.height);
  context.putImageData(new ImageData(rgba, cursor.width, cursor.height), 0, 0);
  return true;
}

/**
 * Where the cursor layer belongs on screen, in client coordinates.
 *
 * The hotspot is the point that has to sit under the pointer, not the shape's
 * top-left: an arrow's hotspot is its tip and an I-beam's is its middle, so
 * placing the bitmap by its corner puts every cursor a few pixels off in a
 * direction that changes with the cursor.
 *
 * The shape is drawn at the picture's own scale, so it grows and shrinks with
 * the display mode rather than staying a fixed size over a zoomed screen.
 */
export function cursorPlacement(
  pointer: { x: number; y: number },
  cursor: CursorShape,
  picture: Box,
  frame: { width: number; height: number },
): { left: number; top: number; width: number; height: number } {
  const scale = frame.width > 0 ? picture.width / frame.width : 1;
  return {
    left: pointer.x - cursor.hotspotX * scale,
    top: pointer.y - cursor.hotspotY * scale,
    width: cursor.width * scale,
    height: cursor.height * scale,
  };
}

/**
 * The `cursor` the picture canvas should carry right now.
 *
 * `none` in exactly one state: the host announced a shape, the operator left
 * the overlay on, and the pointer is over the picture — the layer is already
 * showing where the pointer is, and the system arrow underneath it would be a
 * second cursor. Everything else is the ordinary arrow, including the moment
 * the pointer leaves the picture: `none` left behind there would take the
 * pointer away over the toolbar and the chat panel too.
 *
 * Never `crosshair`. That was for hosts that composite their own cursor into
 * the frame, from before the shape had a channel of its own (ADR 0038); those
 * hosts announce no shape and get the arrow here.
 */
export function cursorCssFor(
  hasShape: boolean,
  localCursor: boolean,
  overPicture: boolean,
): 'none' | 'default' {
  return hasShape && localCursor && overPicture ? 'none' : 'default';
}

/** Where forwarded input events go. The entry point wires this to Tauri IPC. */
export interface InputSink {
  pointerMove(x: number, y: number, modifiers: number): void;
  press(logical: number, scancode: number, modifiers: number, pressed: boolean): void;
  wheel(dx: number, dy: number, modifiers: number): void;
}

/** Modifier bits, matching the order the host reads them back out. */
const MODIFIER_SHIFT = 1;
const MODIFIER_CTRL = 2;
const MODIFIER_ALT = 4;
const MODIFIER_META = 8;

function modifiersOf(event: MouseEvent | KeyboardEvent | WheelEvent): number {
  return (
    (event.shiftKey ? MODIFIER_SHIFT : 0) |
    (event.ctrlKey ? MODIFIER_CTRL : 0) |
    (event.altKey ? MODIFIER_ALT : 0) |
    (event.metaKey ? MODIFIER_META : 0)
  );
}

// A webview reports `KeyboardEvent.key`/`.code`, never a physical scancode, so
// `scancode` is sent as 0 and the host resolves the logical identifier. Single
// characters travel as their code point; named keys get a stable id from this
// table rather than a hash, so the mapping stays reviewable.
const NAMED_KEYS: Readonly<Record<string, number>> = {
  Backspace: 0x08,
  Tab: 0x09,
  Enter: 0x0d,
  Escape: 0x1b,
  Delete: 0x7f,
  ArrowLeft: 0xe000,
  ArrowUp: 0xe001,
  ArrowRight: 0xe002,
  ArrowDown: 0xe003,
  Home: 0xe004,
  End: 0xe005,
  PageUp: 0xe006,
  PageDown: 0xe007,
  Insert: 0xe008,
  Shift: 0xe010,
  Control: 0xe011,
  Alt: 0xe012,
  Meta: 0xe013,
  CapsLock: 0xe014,
};

/** Logical identifier of a keyboard event, or `undefined` if unmappable. */
export function logicalOfKey(key: string): number | undefined {
  const named = NAMED_KEYS[key];
  if (named !== undefined) {
    return named;
  }
  if (key.length === 1 || (key.length === 2 && key.codePointAt(0)! > 0xffff)) {
    return key.codePointAt(0);
  }
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(key)) {
    return 0xe100 + Number(key.slice(1));
  }
  return undefined;
}

/** Logical identifier of a pointer button. */
export function logicalOfButton(button: number): number {
  return POINTER_BUTTON_LOGICAL_BASE + button;
}

/**
 * Stops the local WebView's native context menu from popping up over the
 * remote picture. A right click already reaches the host as an ordinary
 * pointer-button press/release through {@link ViewInput} regardless of this
 * — this only removes the local popup that would otherwise sit on top of
 * whatever the host's own right-click menu shows.
 */
export function suppressContextMenu(target: EventTarget): void {
  target.addEventListener('contextmenu', (event) => event.preventDefault());
}

/**
 * Whether `target` is a local text field this window must not type over.
 *
 * The keyboard listeners sit on the *document*, not on the canvas, because a
 * canvas cannot hold focus and a window that only forwarded what the picture
 * was focused on would forward nothing at all. The cost of that is this
 * function: everything typed anywhere in the window reaches the forwarder,
 * including what is typed into the window's own chat box, and without a guard
 * the chat box receives no characters at all — every one of them is consumed
 * here and sent to the other machine.
 *
 * `event.target` rather than `document.activeElement`: the two agree in a
 * browser but not in every test, and the event's own target is the one that
 * actually produced the keystroke.
 */
export function isLocalTextTarget(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) {
    return false;
  }
  if ((target as HTMLElement).isContentEditable) {
    return true;
  }
  const tag = target.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT';
}

/**
 * Owner of the view window's pointer and keyboard listeners.
 *
 * Listeners are attached on `setEnabled(true)` and fully removed on
 * `setEnabled(false)`: a session whose role was lowered mid-flight must stop
 * *producing* events, not merely have them rejected further down (§8.1).
 *
 * Where the picture is on screen comes from `geometry`, not from the element:
 * the mapping has to agree with whatever the display mode and the pan did, and
 * reading it back off the DOM would tie the one piece of arithmetic that can go
 * silently wrong to a layout engine no test has.
 */
export class ViewInput {
  private attached = false;

  private readonly onPointerMove = (event: PointerEvent): void => {
    const remote = remotePointer(event.clientX, event.clientY, this.geometry());
    // Outside the picture: there is no remote pixel under the pointer, and
    // reporting the nearest edge would be a move the operator did not make.
    if (!remote) {
      return;
    }
    this.sink.pointerMove(remote.x, remote.y, modifiersOf(event));
  };

  private readonly onPointerDown = (event: PointerEvent): void => {
    // The middle button pans locally and never reaches the host, which is
    // what makes panning possible at all: the left button belongs to the
    // remote machine. `defaultPrevented` covers the other pan gesture —
    // space with the left button — which `installPan` claims in the capture
    // phase before this listener runs.
    if (event.button === POINTER_BUTTON_PAN || event.defaultPrevented) {
      return;
    }
    this.sink.press(logicalOfButton(event.button), 0, modifiersOf(event), true);
  };

  private readonly onPointerUp = (event: PointerEvent): void => {
    if (event.button === POINTER_BUTTON_PAN || event.defaultPrevented) {
      return;
    }
    this.sink.press(logicalOfButton(event.button), 0, modifiersOf(event), false);
  };

  private readonly onWheel = (event: WheelEvent): void => {
    event.preventDefault();
    // Ctrl+wheel zooms locally (§11) and is not scrolling the remote machine.
    if (event.ctrlKey) {
      return;
    }
    this.sink.wheel(clampDelta(event.deltaX), clampDelta(event.deltaY), modifiersOf(event));
  };

  private readonly onKeyDown = (event: KeyboardEvent): void => this.forwardKey(event, true);

  private readonly onKeyUp = (event: KeyboardEvent): void => this.forwardKey(event, false);

  constructor(
    private readonly surface: HTMLElement,
    private readonly sink: InputSink,
    private readonly keyboard: EventTarget = surface.ownerDocument ?? document,
    /**
     * Where the picture is right now, in client coordinates.
     *
     * Defaults to the element's own box, which is correct while the picture
     * fills it; a window that zooms and pans supplies {@link pictureBox}
     * instead.
     */
    private readonly geometry: () => Box = () => surface.getBoundingClientRect(),
  ) {}

  get enabled(): boolean {
    return this.attached;
  }

  setEnabled(enabled: boolean): void {
    if (enabled === this.attached) {
      return;
    }
    this.attached = enabled;
    if (enabled) {
      this.surface.addEventListener('pointermove', this.onPointerMove as EventListener);
      this.surface.addEventListener('pointerdown', this.onPointerDown as EventListener);
      this.surface.addEventListener('pointerup', this.onPointerUp as EventListener);
      this.surface.addEventListener('wheel', this.onWheel as EventListener, { passive: false });
      this.keyboard.addEventListener('keydown', this.onKeyDown as EventListener);
      this.keyboard.addEventListener('keyup', this.onKeyUp as EventListener);
      return;
    }
    this.surface.removeEventListener('pointermove', this.onPointerMove as EventListener);
    this.surface.removeEventListener('pointerdown', this.onPointerDown as EventListener);
    this.surface.removeEventListener('pointerup', this.onPointerUp as EventListener);
    this.surface.removeEventListener('wheel', this.onWheel as EventListener);
    this.keyboard.removeEventListener('keydown', this.onKeyDown as EventListener);
    this.keyboard.removeEventListener('keyup', this.onKeyUp as EventListener);
  }

  private forwardKey(event: KeyboardEvent, pressed: boolean): void {
    // A client hotkey has already been handled and marked; forwarding it too
    // would deliver half a chord to the remote machine (§11).
    if (event.defaultPrevented) {
      return;
    }
    // Typed into one of this window's own fields — the chat box. Neither
    // forwarded nor cancelled: the field has to receive the character, and the
    // remote machine has no business seeing it.
    if (isLocalTextTarget(event.target)) {
      return;
    }
    const logical = logicalOfKey(event.key);
    if (logical === undefined) {
      return;
    }
    event.preventDefault();
    this.sink.press(logical, 0, modifiersOf(event), pressed);
  }
}

/** Pointer button that pans locally instead of reaching the host: middle. */
export const POINTER_BUTTON_PAN = 1;

/** What a pan gesture needs to know and to change. */
export interface PanHost {
  /** The layout as it stands. */
  layout(): ViewLayout;
  /** Moves the picture by `dx`/`dy` CSS pixels. */
  panBy(dx: number, dy: number): void;
}

/**
 * Wires local panning onto `surface`: middle-button drag, or space held with
 * the left button.
 *
 * Never the plain left button. That one is the remote machine's, and a pan
 * gesture that stole it would make the picture unusable for the thing it is
 * there for.
 *
 * The pointer half takes nothing from the host: the middle button is not
 * forwarded at all, and a space-drag is claimed in the capture phase so the
 * left press never reaches {@link ViewInput}. The space *key* is a different
 * matter — it is a real key the operator pressed and it still travels, which
 * is why the middle button is the gesture the help text names first.
 *
 * Returns a teardown for the listeners it installs.
 */
export function installPan(surface: HTMLElement, host: PanHost, keyboard: EventTarget): () => void {
  let spaceHeld = false;
  let dragging: { x: number; y: number } | null = null;

  const isPanGesture = (event: PointerEvent): boolean =>
    event.button === POINTER_BUTTON_PAN || (spaceHeld && event.button === 0);

  const onKeyDown = (event: Event): void => {
    if ((event as KeyboardEvent).code === 'Space') {
      spaceHeld = true;
    }
  };
  const onKeyUp = (event: Event): void => {
    if ((event as KeyboardEvent).code === 'Space') {
      spaceHeld = false;
    }
  };
  const onPointerDown = (event: Event): void => {
    const pointer = event as PointerEvent;
    if (!isPanGesture(pointer)) {
      return;
    }
    // Marks the event so the input forwarder skips it: a space-drag that also
    // clicked the remote machine would be worse than no pan at all.
    pointer.preventDefault();
    dragging = { x: pointer.clientX, y: pointer.clientY };
    surface.setPointerCapture?.(pointer.pointerId);
  };
  const onPointerMove = (event: Event): void => {
    if (!dragging) {
      return;
    }
    const pointer = event as PointerEvent;
    host.panBy(pointer.clientX - dragging.x, pointer.clientY - dragging.y);
    dragging = { x: pointer.clientX, y: pointer.clientY };
  };
  const onPointerUp = (event: Event): void => {
    if (!dragging) {
      return;
    }
    // The release that ends a pan is claimed too, or the host would get a
    // button release for a press it never saw.
    event.preventDefault();
    dragging = null;
  };

  // Capture phase for the two that have to beat the input forwarder attached
  // to the canvas underneath; the rest are ordinary.
  keyboard.addEventListener('keydown', onKeyDown);
  keyboard.addEventListener('keyup', onKeyUp);
  surface.addEventListener('pointerdown', onPointerDown, true);
  surface.addEventListener('pointermove', onPointerMove);
  surface.addEventListener('pointerup', onPointerUp, true);
  surface.addEventListener('pointercancel', onPointerUp, true);

  return () => {
    keyboard.removeEventListener('keydown', onKeyDown);
    keyboard.removeEventListener('keyup', onKeyUp);
    surface.removeEventListener('pointerdown', onPointerDown, true);
    surface.removeEventListener('pointermove', onPointerMove);
    surface.removeEventListener('pointerup', onPointerUp, true);
    surface.removeEventListener('pointercancel', onPointerUp, true);
  };
}

function clampCoordinate(value: number): number {
  return Math.max(0, Math.min(POINTER_RANGE, Math.round(value)));
}

function clampDelta(value: number): number {
  return Math.max(-32768, Math.min(32767, Math.round(value)));
}

/**
 * Status overlay of the view window.
 *
 * `waiting` and `reconnecting` are inline and non-blocking — the last picture
 * stays visible underneath, because neither is a revoke. The terminal states
 * are modals, which is what the connection-health policy calls for: the only
 * way out of one is ending the session, which is what closing the window
 * already means.
 *
 * The two "no picture" states get their own text rather than reusing
 * `failed`: telling someone the connection was lost when the host simply has
 * no capture backend sends them to debug a network that is working fine.
 */
export function viewOverlay(status: ViewStatus, locale: Locale, onDismiss: () => void): TemplateResult {
  if (status === 'live') {
    return html``;
  }
  if (status === 'failed') {
    return terminalModal(
      t(locale, 'view.failed.title'),
      t(locale, 'view.failed.body'),
      t(locale, 'view.failed.dismiss'),
      onDismiss,
    );
  }
  if (status === 'no-capture' || status === 'no-encoder') {
    return terminalModal(
      t(locale, 'view.unavailable.title'),
      t(locale, status === 'no-capture' ? 'view.unavailable.noCapture' : 'view.unavailable.noEncoder'),
      t(locale, 'view.unavailable.dismiss'),
      onDismiss,
    );
  }
  const key = status === 'waiting' ? 'view.waiting' : 'view.reconnecting';
  return html`<p class="view-banner" role="status" aria-live="polite">${t(locale, key)}</p>`;
}

/**
 * The recording badge of §17, shown while the host says it is recording.
 *
 * Separate from the toolbar on purpose: the toolbar collapses and can be
 * dragged out of the way, and an indicator someone can put away is not an
 * indicator. This one only goes away when the recording does.
 */
export function recordingBadge(recording: boolean, locale: Locale): TemplateResult {
  if (!recording) {
    return html``;
  }
  return html`
    <p class="view-recording" role="status" aria-live="polite" data-testid="view-recording">
      <span class="view-recording-dot" aria-hidden="true"></span>
      ${t(locale, 'view.recording')}
    </p>
  `;
}

function terminalModal(
  title: string,
  body: string,
  dismiss: string,
  onDismiss: () => void,
): TemplateResult {
  return html`
    <div class="view-modal" role="alertdialog" aria-modal="true" aria-labelledby="view-error-title">
      <h2 id="view-error-title">${title}</h2>
      <p>${body}</p>
      <button type="button" autofocus @click=${onDismiss}>${dismiss}</button>
    </div>
  `;
}
