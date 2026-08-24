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

/** Logical identifiers at or above this value are pointer buttons, not keys. */
export const POINTER_BUTTON_LOGICAL_BASE = 0xf0000000;

/** Normalized pointer coordinate space of the protocol: 0..=65535. */
const POINTER_RANGE = 65535;

export interface ViewFrame {
  status: ViewStatus;
  /** Whether the session's `input` grant is live right now. */
  input: boolean;
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
 * `status:u8 | input:u8 | width:u32 | height:u32 | timestamp_us:u64 | RGBA8`.
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
  return {
    status,
    input: header.getUint8(1) === 1,
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
 * Owner of the view window's pointer and keyboard listeners.
 *
 * Listeners are attached on `setEnabled(true)` and fully removed on
 * `setEnabled(false)`: a session whose role was lowered mid-flight must stop
 * *producing* events, not merely have them rejected further down (§8.1).
 */
export class ViewInput {
  private attached = false;

  private readonly onPointerMove = (event: PointerEvent): void => {
    const rect = this.surface.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) {
      return;
    }
    const x = ((event.clientX - rect.left) / rect.width) * POINTER_RANGE;
    const y = ((event.clientY - rect.top) / rect.height) * POINTER_RANGE;
    this.sink.pointerMove(clampCoordinate(x), clampCoordinate(y), modifiersOf(event));
  };

  private readonly onPointerDown = (event: PointerEvent): void => {
    this.sink.press(logicalOfButton(event.button), 0, modifiersOf(event), true);
  };

  private readonly onPointerUp = (event: PointerEvent): void => {
    this.sink.press(logicalOfButton(event.button), 0, modifiersOf(event), false);
  };

  private readonly onWheel = (event: WheelEvent): void => {
    event.preventDefault();
    this.sink.wheel(clampDelta(event.deltaX), clampDelta(event.deltaY), modifiersOf(event));
  };

  private readonly onKeyDown = (event: KeyboardEvent): void => this.forwardKey(event, true);

  private readonly onKeyUp = (event: KeyboardEvent): void => this.forwardKey(event, false);

  constructor(
    private readonly surface: HTMLElement,
    private readonly sink: InputSink,
    private readonly keyboard: EventTarget = surface.ownerDocument ?? document,
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
    const logical = logicalOfKey(event.key);
    if (logical === undefined) {
      return;
    }
    event.preventDefault();
    this.sink.press(logical, 0, modifiersOf(event), pressed);
  }
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
