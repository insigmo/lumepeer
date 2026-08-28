// Remote-view window (design doc §11, §13).
//
// Two things are worth pinning here. The first is that a fixture frame coming
// off the binary IPC boundary actually reaches the canvas — the whole window is
// one `putImageData` call and a wrong header offset would silently paint
// garbage. The second is the §2.2 rule that matters most in this window:
// pointer and keyboard listeners exist only while the `input` grant is live,
// and a grant that drops takes them away again rather than merely having the
// events rejected downstream.
import * as axe from 'axe-core';
import { render } from 'lit-html';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import viewMarkup from '../view.html?raw';
import { SUPPORTED_LOCALES } from './i18n';
import {
  clampPan,
  cursorCssFor,
  cursorPlacement,
  CURSOR_RESPONSE_HEADER_BYTES,
  decodeCursorShape,
  decodeViewFrame,
  defaultLayout,
  displaySize,
  effectiveScale,
  installPan,
  isLocalTextTarget,
  logicalOfButton,
  logicalOfKey,
  MAX_SCALE,
  nextDisplayMode,
  paintCursor,
  paintFrame,
  pictureBox,
  POINTER_BUTTON_LOGICAL_BASE,
  POINTER_BUTTON_PAN,
  remotePointer,
  suppressContextMenu,
  ViewInput,
  viewOverlay,
  VIEW_RESPONSE_HEADER_BYTES,
  zoomBy,
  type Box,
  type CursorShape,
  type InputSink,
  type ViewLayout,
  type ViewStatus,
} from './view-window';

const LAYOUT_DEPENDENT_RULES = ['color-contrast', 'target-size'];

// jsdom ships no canvas backend, so neither `ImageData` nor a 2D context
// exists here. Both are stubbed rather than skipped: what this file can still
// prove is that the bytes crossing the IPC boundary reach `putImageData`
// unchanged and at the right dimensions, which is exactly where an off-by-one
// header offset would show up. Pixel-accurate rendering is the browser's job.
if (typeof globalThis.ImageData === 'undefined') {
  class StubImageData {
    readonly colorSpace = 'srgb' as const;
    constructor(
      readonly data: Uint8ClampedArray,
      readonly width: number,
      readonly height: number,
    ) {}
  }
  globalThis.ImageData = StubImageData as unknown as typeof ImageData;
}

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

/** Builds the exact bytes `view_next_frame` returns. */
function response(options: {
  status: number;
  input: boolean;
  width: number;
  height: number;
  timestampUs?: bigint;
  pixels?: Uint8Array;
}): ArrayBuffer {
  const pixels = options.pixels ?? new Uint8Array(0);
  const buffer = new ArrayBuffer(VIEW_RESPONSE_HEADER_BYTES + pixels.length);
  const view = new DataView(buffer);
  view.setUint8(0, options.status);
  view.setUint8(1, options.input ? 1 : 0);
  view.setUint32(2, options.width, true);
  view.setUint32(6, options.height, true);
  view.setBigUint64(10, options.timestampUs ?? 0n, true);
  new Uint8Array(buffer, VIEW_RESPONSE_HEADER_BYTES).set(pixels);
  return buffer;
}

function recordingSink(): { sink: InputSink; calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    sink: {
      pointerMove: (x, y, modifiers) => calls.push(`move ${x} ${y} ${modifiers}`),
      press: (logical, scancode, modifiers, pressed) =>
        calls.push(`press ${logical} ${scancode} ${modifiers} ${pressed}`),
      wheel: (dx, dy, modifiers) => calls.push(`wheel ${dx} ${dy} ${modifiers}`),
    },
  };
}

describe('view window: frame decoding', () => {
  it('reads the header of a response that carries no picture yet', () => {
    const frame = decodeViewFrame(response({ status: 0, input: false, width: 0, height: 0 }));
    expect(frame.status).toBe('waiting');
    expect(frame.input).toBe(false);
    expect(frame.pixels).toHaveLength(0);
  });

  it('reads status, live input grant, dimensions, timestamp and pixels', () => {
    const pixels = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]);
    const frame = decodeViewFrame(
      response({ status: 1, input: true, width: 2, height: 1, timestampUs: 123n, pixels }),
    );
    expect(frame.status).toBe('live');
    expect(frame.input).toBe(true);
    expect(frame.width).toBe(2);
    expect(frame.height).toBe(1);
    expect(frame.timestampUs).toBe(123);
    expect(Array.from(frame.pixels)).toEqual(Array.from(pixels));
  });

  it('refuses a truncated response instead of painting garbage', () => {
    expect(() => decodeViewFrame(new ArrayBuffer(4))).toThrow();
  });

  // Codes 4 and 5 are `ViewStatus::NoCapture` / `NoEncoder` in view.rs.
  it('decodes the two host-fault statuses', () => {
    expect(decodeViewFrame(response({ status: 4, input: false, width: 0, height: 0 })).status).toBe('no-capture');
    expect(decodeViewFrame(response({ status: 5, input: false, width: 0, height: 0 })).status).toBe('no-encoder');
  });

  it('refuses an unknown status byte', () => {
    expect(() => decodeViewFrame(response({ status: 9, input: false, width: 0, height: 0 }))).toThrow();
  });
});

describe('view window: canvas render', () => {
  it('paints a fixture frame at its own resolution', () => {
    const canvas = document.createElement('canvas');
    const putImageData = vi.fn();
    // jsdom has no 2D backend; the contract under test is what we hand it.
    vi.spyOn(canvas, 'getContext').mockReturnValue({ putImageData } as unknown as CanvasRenderingContext2D);

    const pixels = new Uint8Array(2 * 1 * 4).fill(0x7f);
    const frame = decodeViewFrame(response({ status: 1, input: false, width: 2, height: 1, pixels }));

    expect(paintFrame(canvas, frame)).toBe(true);
    expect(canvas.width).toBe(2);
    expect(canvas.height).toBe(1);
    expect(putImageData).toHaveBeenCalledOnce();
    const painted = putImageData.mock.calls[0]?.[0] as ImageData;
    expect(painted.width).toBe(2);
    expect(painted.height).toBe(1);
    expect(Array.from(painted.data)).toEqual(Array.from(pixels));
  });

  it('follows the remote screen changing resolution mid-session', () => {
    const canvas = document.createElement('canvas');
    const putImageData = vi.fn();
    vi.spyOn(canvas, 'getContext').mockReturnValue({ putImageData } as unknown as CanvasRenderingContext2D);

    const paint = (width: number, height: number): boolean =>
      paintFrame(
        canvas,
        decodeViewFrame(
          response({
            status: 1,
            input: false,
            width,
            height,
            pixels: new Uint8Array(width * height * 4).fill(0x40),
          }),
        ),
      );

    expect(paint(4, 2)).toBe(true);
    expect([canvas.width, canvas.height]).toEqual([4, 2]);
    // A host that switches resolution hands back a differently shaped buffer
    // on the very next frame; the backing store follows it rather than
    // painting the new pixels into the old geometry.
    expect(paint(2, 4)).toBe(true);
    expect([canvas.width, canvas.height]).toEqual([2, 4]);
    const painted = putImageData.mock.calls.at(-1)?.[0] as ImageData;
    expect([painted.width, painted.height]).toEqual([2, 4]);
    expect(paint(4, 2)).toBe(true);
    expect([canvas.width, canvas.height]).toEqual([4, 2]);
  });

  it('paints nothing when no picture has arrived', () => {
    const canvas = document.createElement('canvas');
    const frame = decodeViewFrame(response({ status: 0, input: false, width: 0, height: 0 }));
    expect(paintFrame(canvas, frame)).toBe(false);
  });

  it('paints nothing when the pixel buffer is shorter than the announced size', () => {
    const canvas = document.createElement('canvas');
    const frame = decodeViewFrame(
      response({ status: 1, input: false, width: 4, height: 4, pixels: new Uint8Array(8) }),
    );
    expect(paintFrame(canvas, frame)).toBe(false);
  });
});

describe('view window: input listeners follow the live grant', () => {
  function surface(): { canvas: HTMLCanvasElement; input: ViewInput; calls: string[] } {
    const canvas = document.createElement('canvas');
    canvas.width = 100;
    canvas.height = 100;
    container.appendChild(canvas);
    canvas.getBoundingClientRect = () =>
      ({ left: 0, top: 0, width: 100, height: 100, right: 100, bottom: 100 }) as DOMRect;
    const { sink, calls } = recordingSink();
    const geometry = (): Box => ({ left: 0, top: 0, width: 100, height: 100 });
    return { canvas, input: new ViewInput(canvas, sink, container, geometry), calls };
  }

  it('attaches nothing until the input grant is live', () => {
    const { canvas, input, calls } = surface();
    expect(input.enabled).toBe(false);

    canvas.dispatchEvent(new MouseEvent('pointermove', { clientX: 50, clientY: 50, bubbles: true }));
    canvas.dispatchEvent(new MouseEvent('pointerdown', { button: 0, bubbles: true }));
    canvas.dispatchEvent(new WheelEvent('wheel', { deltaY: 3, bubbles: true }));
    container.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', bubbles: true }));

    expect(calls).toEqual([]);
  });

  it('forwards pointer, wheel and keyboard events once the grant carries input', () => {
    const { canvas, input, calls } = surface();
    input.setEnabled(true);
    expect(input.enabled).toBe(true);

    canvas.dispatchEvent(new MouseEvent('pointermove', { clientX: 50, clientY: 25, bubbles: true }));
    canvas.dispatchEvent(new MouseEvent('pointerdown', { button: 0, bubbles: true }));
    canvas.dispatchEvent(new MouseEvent('pointerup', { button: 0, bubbles: true }));
    canvas.dispatchEvent(new WheelEvent('wheel', { deltaX: 1, deltaY: -2, bubbles: true }));
    container.dispatchEvent(new KeyboardEvent('keydown', { key: 'a', bubbles: true }));
    container.dispatchEvent(new KeyboardEvent('keyup', { key: 'a', bubbles: true }));

    expect(calls).toEqual([
      // Half the width and a quarter of the height of the 0..=65535 space.
      'move 32768 16384 0',
      `press ${POINTER_BUTTON_LOGICAL_BASE} 0 0 true`,
      `press ${POINTER_BUTTON_LOGICAL_BASE} 0 0 false`,
      'wheel 1 -2 0',
      'press 97 0 0 true',
      'press 97 0 0 false',
    ]);
  });

  it('removes every listener again when the grant drops input mid-session', () => {
    const { canvas, input, calls } = surface();
    input.setEnabled(true);
    canvas.dispatchEvent(new MouseEvent('pointermove', { clientX: 0, clientY: 0, bubbles: true }));
    expect(calls).toHaveLength(1);

    input.setEnabled(false);
    expect(input.enabled).toBe(false);
    canvas.dispatchEvent(new MouseEvent('pointermove', { clientX: 50, clientY: 50, bubbles: true }));
    canvas.dispatchEvent(new MouseEvent('pointerdown', { button: 2, bubbles: true }));
    canvas.dispatchEvent(new WheelEvent('wheel', { deltaY: 1, bubbles: true }));
    container.dispatchEvent(new KeyboardEvent('keydown', { key: 'b', bubbles: true }));

    expect(calls).toHaveLength(1);
  });

  it('carries the modifier bitmask and maps named keys and buttons', () => {
    const { canvas, input, calls } = surface();
    input.setEnabled(true);
    container.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', ctrlKey: true, bubbles: true }));
    canvas.dispatchEvent(new MouseEvent('pointerdown', { button: 2, shiftKey: true, bubbles: true }));

    expect(calls).toEqual([
      `press ${logicalOfKey('Enter')} 0 2 true`,
      `press ${logicalOfButton(2)} 0 1 true`,
    ]);
  });

  it('keeps the middle button local: it pans, and never reaches the host', () => {
    const { canvas, input, calls } = surface();
    input.setEnabled(true);
    canvas.dispatchEvent(
      new MouseEvent('pointerdown', { button: POINTER_BUTTON_PAN, bubbles: true }),
    );
    canvas.dispatchEvent(new MouseEvent('pointerup', { button: POINTER_BUTTON_PAN, bubbles: true }));
    expect(calls).toEqual([]);
  });

  it('keeps Ctrl+wheel local: it zooms, and is not a scroll on the host', () => {
    const { canvas, input, calls } = surface();
    input.setEnabled(true);
    canvas.dispatchEvent(new WheelEvent('wheel', { deltaY: -3, ctrlKey: true, bubbles: true }));
    expect(calls).toEqual([]);
    canvas.dispatchEvent(new WheelEvent('wheel', { deltaY: -3, bubbles: true }));
    expect(calls).toEqual(['wheel 0 -3 0']);
  });

  it('drops keys it cannot map rather than inventing an identifier', () => {
    const { input, calls } = surface();
    input.setEnabled(true);
    container.dispatchEvent(new KeyboardEvent('keydown', { key: 'BrightnessUp', bubbles: true }));
    expect(calls).toEqual([]);
  });

  // The listeners sit on the document, not on the canvas, so everything typed
  // anywhere in the window reaches them — including what is typed into the
  // window's own chat box. Without the guard the box receives nothing at all:
  // every character is cancelled here and sent to the other machine instead.
  it('leaves a local text field alone: the field gets the key, the host does not', () => {
    const { input, calls } = surface();
    input.setEnabled(true);
    const field = document.createElement('input');
    container.appendChild(field);

    const typed = new KeyboardEvent('keydown', { key: 'a', bubbles: true, cancelable: true });
    field.dispatchEvent(typed);
    expect(calls).toEqual([]);
    expect(typed.defaultPrevented).toBe(false);

    const onPicture = new KeyboardEvent('keydown', { key: 'a', bubbles: true, cancelable: true });
    container.dispatchEvent(onPicture);
    expect(calls).toEqual(['press 97 0 0 true']);
    expect(onPicture.defaultPrevented).toBe(true);
  });
});

// The window's own stylesheet, which is where two of its rules actually live:
// `#chat-panel` and `#cursor` both declare a `display`, and an id selector
// beats the browser's own `[hidden] { display: none }`. Nothing in TypeScript
// can compensate for that, so the sheet itself is what gets asserted.
describe('view window: the panels can be hidden at all', () => {
  const markup = viewMarkup;

  beforeEach(() => {
    const css = /<style>([\s\S]*?)<\/style>/.exec(markup)?.[1] ?? '';
    expect(css).not.toBe('');
    const style = document.createElement('style');
    style.textContent = css;
    container.appendChild(style);
  });

  it('hides the chat panel when the attribute is set, and shows it when it is not', () => {
    const panel = document.createElement('aside');
    panel.id = 'chat-panel';
    container.appendChild(panel);

    expect(getComputedStyle(panel).display).toBe('flex');
    panel.hidden = true;
    expect(getComputedStyle(panel).display).toBe('none');
  });

  it('ships the panel hidden, so the window opens on the picture', () => {
    expect(/<aside id="chat-panel"[^>]*\shidden\s*>/.test(markup)).toBe(true);
  });
});

describe('view window: local text targets', () => {
  it('names every field a keystroke belongs to', () => {
    for (const tag of ['input', 'textarea', 'select']) {
      expect(isLocalTextTarget(document.createElement(tag))).toBe(true);
    }
    const editable = document.createElement('div');
    // jsdom implements `isContentEditable` as a stub that never turns true, so
    // the property is the thing under test and is set directly.
    Object.defineProperty(editable, 'isContentEditable', { value: true });
    expect(isLocalTextTarget(editable)).toBe(true);
  });

  it('leaves everything else to the remote machine', () => {
    expect(isLocalTextTarget(document.createElement('canvas'))).toBe(false);
    expect(isLocalTextTarget(document.createElement('div'))).toBe(false);
    expect(isLocalTextTarget(document.body)).toBe(false);
    expect(isLocalTextTarget(null)).toBe(false);
    expect(isLocalTextTarget(new EventTarget())).toBe(false);
  });
});

describe('view window: display modes', () => {
  const frame = { width: 1920, height: 1080 };
  const viewport: Box = { left: 0, top: 0, width: 960, height: 720 };

  it('fits the picture to the window, keeping its aspect ratio', () => {
    const size = displaySize({ ...defaultLayout(), mode: 'fit' }, frame, viewport, 1);
    // The width is the binding axis for a 16:9 picture in a 4:3 window.
    expect(size.width).toBeCloseTo(960);
    expect(size.height).toBeCloseTo(540);
    expect(size.width / size.height).toBeCloseTo(frame.width / frame.height);
  });

  it('makes 1:1 mean one frame pixel per DEVICE pixel, not per CSS pixel', () => {
    const layout: ViewLayout = { ...defaultLayout(), mode: 'actual' };
    expect(effectiveScale(layout, frame, viewport, 2)).toBe(1);
    // At a 2x ratio the element is half as wide in CSS pixels, which is
    // exactly what puts one frame pixel on one physical pixel. Sizing it at
    // 1920 CSS pixels would draw the picture at half its real resolution and
    // call that "1:1".
    expect(displaySize(layout, frame, viewport, 2).width).toBeCloseTo(960);
    expect(displaySize(layout, frame, viewport, 1).width).toBeCloseTo(1920);
  });

  it('zooms from whatever the eye was already on, and stops at the bounds', () => {
    const fit = effectiveScale(defaultLayout(), frame, viewport, 1);
    const zoomed = zoomBy(defaultLayout(), 1, frame, viewport, 1);
    expect(zoomed.mode).toBe('scaled');
    expect(zoomed.scale).toBeGreaterThan(fit);

    let layout = defaultLayout();
    for (let i = 0; i < 100; i += 1) {
      layout = zoomBy(layout, 1, frame, viewport, 1);
    }
    expect(layout.scale).toBe(MAX_SCALE);
  });

  it('cycles the modes and comes back round', () => {
    expect(nextDisplayMode('fit')).toBe('actual');
    expect(nextDisplayMode('actual')).toBe('scaled');
    expect(nextDisplayMode('scaled')).toBe('fit');
  });

  it('refuses to pan a picture that already fits, and bounds one that does not', () => {
    const fitted = clampPan(
      { ...defaultLayout(), offsetX: 400, offsetY: 400 },
      frame,
      viewport,
      1,
    );
    expect(fitted.offsetX).toBe(0);
    expect(fitted.offsetY).toBe(0);

    // At 1:1 the picture is 1920 wide in a 960 window: half the overhang each
    // way, and never further, or the operator pans it off the screen.
    const panned = clampPan(
      { mode: 'actual', scale: 1, offsetX: 9999, offsetY: -9999 },
      frame,
      viewport,
      1,
    );
    expect(panned.offsetX).toBeCloseTo((1920 - 960) / 2);
    expect(panned.offsetY).toBeCloseTo(-(1080 - 720) / 2);
  });
});

describe('view window: pointer mapping', () => {
  // The mapping that goes silently wrong the moment the picture stops filling
  // the window. One case per display mode, all the way from a layout to the
  // normalized coordinate the host receives.
  const frame = { width: 1920, height: 1080 };
  const viewport: Box = { left: 0, top: 0, width: 960, height: 720 };

  function centreOf(layout: ViewLayout, ratio = 1): { x: number; y: number } | null {
    const picture = pictureBox(layout, frame, viewport, ratio);
    return remotePointer(
      picture.left + picture.width / 2,
      picture.top + picture.height / 2,
      picture,
    );
  }

  it('fit: the middle of the picture is the middle of the remote screen', () => {
    expect(centreOf(defaultLayout())).toEqual({ x: 32768, y: 32768 });
  });

  it('fit: the corners are the corners', () => {
    const picture = pictureBox(defaultLayout(), frame, viewport, 1);
    expect(remotePointer(picture.left, picture.top, picture)).toEqual({ x: 0, y: 0 });
    expect(
      remotePointer(picture.left + picture.width, picture.top + picture.height, picture),
    ).toEqual({ x: 65535, y: 65535 });
  });

  it('1:1: the middle is still the middle even though the picture overflows', () => {
    expect(centreOf({ ...defaultLayout(), mode: 'actual' })).toEqual({ x: 32768, y: 32768 });
  });

  it('zoomed and panned: the pan is subtracted rather than ignored', () => {
    const layout: ViewLayout = { mode: 'scaled', scale: 2, offsetX: 120, offsetY: -80 };
    expect(centreOf(layout)).toEqual({ x: 32768, y: 32768 });

    // And the same screen point means something different once panned, which
    // is the exact bug a rect-free mapping would have hidden.
    const centred = pictureBox({ ...layout, offsetX: 0, offsetY: 0 }, frame, viewport, 1);
    const panned = pictureBox(layout, frame, viewport, 1);
    const screenPoint = { x: 480, y: 360 };
    expect(remotePointer(screenPoint.x, screenPoint.y, panned)).not.toEqual(
      remotePointer(screenPoint.x, screenPoint.y, centred),
    );
  });

  it('HiDPI: the ratio changes the CSS size but not the remote coordinate', () => {
    const layout: ViewLayout = { ...defaultLayout(), mode: 'actual' };
    expect(centreOf(layout, 2)).toEqual(centreOf(layout, 1));
    expect(centreOf({ ...defaultLayout(), mode: 'fit' }, 2)).toEqual({ x: 32768, y: 32768 });
  });

  it('reports nothing for a point outside the picture', () => {
    const picture = pictureBox({ ...defaultLayout(), mode: 'actual' }, frame, viewport, 1);
    // Left of the picture, which at 1:1 hangs off both sides of the window.
    expect(remotePointer(picture.left - 1, picture.top + 10, picture)).toBeNull();
    expect(remotePointer(picture.left + 10, picture.top - 1, picture)).toBeNull();
  });

  it('reports nothing at all before a picture has a size', () => {
    expect(remotePointer(0, 0, { left: 0, top: 0, width: 0, height: 0 })).toBeNull();
  });
});

describe('view window: panning', () => {
  function panSurface(): {
    surface: HTMLElement;
    layout: ViewLayout;
    stop: () => void;
  } {
    const surface = document.createElement('div');
    container.appendChild(surface);
    const state = { layout: defaultLayout() };
    const stop = installPan(
      surface,
      {
        layout: () => state.layout,
        panBy: (dx, dy) => {
          state.layout = {
            ...state.layout,
            offsetX: state.layout.offsetX + dx,
            offsetY: state.layout.offsetY + dy,
          };
        },
      },
      container,
    );
    return {
      surface,
      get layout(): ViewLayout {
        return state.layout;
      },
      stop,
    };
  }

  it('pans on the middle button', () => {
    const pan = panSurface();
    try {
      pan.surface.dispatchEvent(
        new MouseEvent('pointerdown', { button: POINTER_BUTTON_PAN, clientX: 10, clientY: 10 }),
      );
      pan.surface.dispatchEvent(new MouseEvent('pointermove', { clientX: 40, clientY: 25 }));
      expect(pan.layout.offsetX).toBe(30);
      expect(pan.layout.offsetY).toBe(15);
      pan.surface.dispatchEvent(new MouseEvent('pointerup', {}));
      pan.surface.dispatchEvent(new MouseEvent('pointermove', { clientX: 400, clientY: 400 }));
      expect(pan.layout.offsetX).toBe(30);
    } finally {
      pan.stop();
    }
  });

  it('claims the space-drag so the same press never reaches the host as a click', () => {
    const pan = panSurface();
    try {
      container.dispatchEvent(new KeyboardEvent('keydown', { code: 'Space' }));
      const press = new MouseEvent('pointerdown', {
        button: 0,
        clientX: 10,
        clientY: 10,
        cancelable: true,
      });
      pan.surface.dispatchEvent(press);
      expect(press.defaultPrevented).toBe(true);
      const release = new MouseEvent('pointerup', { button: 0, cancelable: true });
      pan.surface.dispatchEvent(release);
      expect(release.defaultPrevented).toBe(true);
    } finally {
      pan.stop();
    }
  });

  it('never pans on the plain left button, which belongs to the remote machine', () => {
    const pan = panSurface();
    try {
      pan.surface.dispatchEvent(
        new MouseEvent('pointerdown', { button: 0, clientX: 10, clientY: 10 }),
      );
      pan.surface.dispatchEvent(new MouseEvent('pointermove', { clientX: 400, clientY: 400 }));
      expect(pan.layout.offsetX).toBe(0);
      expect(pan.layout.offsetY).toBe(0);
    } finally {
      pan.stop();
    }
  });

  it('pans on space plus the left button, and stops again when space is let go', () => {
    const pan = panSurface();
    try {
      container.dispatchEvent(new KeyboardEvent('keydown', { code: 'Space' }));
      pan.surface.dispatchEvent(
        new MouseEvent('pointerdown', { button: 0, clientX: 10, clientY: 10 }),
      );
      pan.surface.dispatchEvent(new MouseEvent('pointermove', { clientX: 30, clientY: 10 }));
      expect(pan.layout.offsetX).toBe(20);
      pan.surface.dispatchEvent(new MouseEvent('pointerup', {}));

      container.dispatchEvent(new KeyboardEvent('keyup', { code: 'Space' }));
      pan.surface.dispatchEvent(
        new MouseEvent('pointerdown', { button: 0, clientX: 10, clientY: 10 }),
      );
      pan.surface.dispatchEvent(new MouseEvent('pointermove', { clientX: 400, clientY: 10 }));
      expect(pan.layout.offsetX).toBe(20);
    } finally {
      pan.stop();
    }
  });
});

describe('view window: the host cursor', () => {
  /** Builds the exact bytes `view_cursor` returns. */
  function cursorResponse(options: {
    seq: number;
    width: number;
    height: number;
    hotspotX?: number;
    hotspotY?: number;
    pixels?: Uint8Array;
  }): ArrayBuffer {
    const pixels = options.pixels ?? new Uint8Array(0);
    const buffer = new ArrayBuffer(CURSOR_RESPONSE_HEADER_BYTES + pixels.length);
    const view = new DataView(buffer);
    view.setUint32(0, options.seq, true);
    view.setUint16(4, options.width, true);
    view.setUint16(6, options.height, true);
    view.setUint16(8, options.hotspotX ?? 0, true);
    view.setUint16(10, options.hotspotY ?? 0, true);
    new Uint8Array(buffer, CURSOR_RESPONSE_HEADER_BYTES).set(pixels);
    return buffer;
  }

  it('reads a header that says this host draws the cursor into the picture', () => {
    const cursor = decodeCursorShape(cursorResponse({ seq: 0, width: 0, height: 0 }));
    expect(cursor.seq).toBe(0);
    expect(cursor.pixels).toHaveLength(0);
  });

  it('reads geometry, hotspot and pixels', () => {
    const pixels = new Uint8Array([1, 2, 3, 255, 4, 5, 6, 255]);
    const cursor = decodeCursorShape(
      cursorResponse({ seq: 7, width: 2, height: 1, hotspotX: 1, hotspotY: 0, pixels }),
    );
    expect(cursor.seq).toBe(7);
    expect([cursor.width, cursor.height]).toEqual([2, 1]);
    expect([cursor.hotspotX, cursor.hotspotY]).toEqual([1, 0]);
    expect(Array.from(cursor.pixels)).toEqual(Array.from(pixels));
  });

  it('refuses a truncated response instead of painting garbage', () => {
    expect(() => decodeCursorShape(new ArrayBuffer(4))).toThrow();
  });

  it('swaps BGRA to RGBA and divides the premultiplied colour back out', () => {
    const canvas = document.createElement('canvas');
    const putImageData = vi.fn();
    vi.spyOn(canvas, 'getContext').mockReturnValue({
      putImageData,
      clearRect: vi.fn(),
    } as unknown as CanvasRenderingContext2D);

    // One premultiplied half-transparent red pixel: B=0, G=0, R=128, A=128.
    const cursor = decodeCursorShape(
      cursorResponse({
        seq: 1,
        width: 1,
        height: 1,
        pixels: new Uint8Array([0, 0, 128, 128]),
      }),
    );
    expect(paintCursor(canvas, cursor)).toBe(true);
    const painted = putImageData.mock.calls[0]?.[0] as ImageData;
    // Straight RGBA: full red at half alpha. Painting the premultiplied bytes
    // straight through would give a half-dark red instead.
    expect(Array.from(painted.data)).toEqual([255, 0, 0, 128]);
  });

  it('paints nothing when the pixel buffer is shorter than the geometry', () => {
    const canvas = document.createElement('canvas');
    const cursor = decodeCursorShape(
      cursorResponse({ seq: 1, width: 4, height: 4, pixels: new Uint8Array(8) }),
    );
    expect(paintCursor(canvas, cursor)).toBe(false);
  });

  it('places the cursor by its hotspot, not by the corner of its bitmap', () => {
    // The claim that matters: the pixel under the pointer is the hotspot. An
    // arrow's hotspot is its tip and an I-beam's is its middle, so placing by
    // the corner is wrong by a different amount for every cursor.
    const cursor: CursorShape = {
      seq: 1,
      width: 32,
      height: 32,
      hotspotX: 16,
      hotspotY: 16,
      pixels: new Uint8ClampedArray(32 * 32 * 4),
    };
    const picture: Box = { left: 0, top: 0, width: 1920, height: 1080 };
    const frame = { width: 1920, height: 1080 };
    const placed = cursorPlacement({ x: 500, y: 400 }, cursor, picture, frame);
    expect(placed.left).toBe(500 - 16);
    expect(placed.top).toBe(400 - 16);
    expect([placed.width, placed.height]).toEqual([32, 32]);

    // A corner hotspot lands the bitmap's corner on the pointer, which is the
    // case that would look correct even with the bug.
    const corner = cursorPlacement(
      { x: 500, y: 400 },
      { ...cursor, hotspotX: 0, hotspotY: 0 },
      picture,
      frame,
    );
    expect([corner.left, corner.top]).toEqual([500, 400]);
  });

  it('grows and shrinks the cursor with the picture it sits on', () => {
    const cursor: CursorShape = {
      seq: 1,
      width: 32,
      height: 32,
      hotspotX: 16,
      hotspotY: 16,
      pixels: new Uint8ClampedArray(32 * 32 * 4),
    };
    const frame = { width: 1920, height: 1080 };
    // The picture drawn at half size: so is the cursor, and so is the hotspot
    // offset, or the two would drift apart as the operator zooms.
    const half = cursorPlacement(
      { x: 500, y: 400 },
      cursor,
      { left: 0, top: 0, width: 960, height: 540 },
      frame,
    );
    expect([half.width, half.height]).toEqual([16, 16]);
    expect(half.left).toBe(500 - 8);
    expect(half.top).toBe(400 - 8);
  });
});

describe('view window: the pointer over the picture', () => {
  // The window used to force `cursor: crosshair` on the canvas, from before
  // the cursor channel existed (ADR 0038). With the host's shape drawn on its
  // own layer that is a second pointer on screen, which is exactly what
  // docs/tasks/09-cursor-shape.md says not to have.
  it('hides the system pointer only while the host shape is the one being drawn', () => {
    expect(cursorCssFor(true, true, true)).toBe('none');
  });

  it('shows the ordinary arrow when the operator turned the overlay off', () => {
    expect(cursorCssFor(true, false, true)).toBe('default');
  });

  it('shows the ordinary arrow on a host that draws its cursor into the frame', () => {
    expect(cursorCssFor(false, true, true)).toBe('default');
    expect(cursorCssFor(false, false, true)).toBe('default');
  });

  // The dangerous one: leaving `none` behind when the pointer moves off the
  // picture takes the pointer away over the toolbar and the chat panel, and
  // the window stops being operable.
  it('brings the arrow back as soon as the pointer leaves the picture', () => {
    expect(cursorCssFor(true, true, false)).toBe('default');
    expect(cursorCssFor(false, true, false)).toBe('default');
  });
});

describe('view window: context menu', () => {
  it('prevents the native context menu from opening', () => {
    const target = document.createElement('div');
    suppressContextMenu(target);
    const event = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
    target.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
  });
});

describe('view window: status overlay', () => {
  const noop = (): void => {};

  it('shows nothing over a live stream', () => {
    render(viewOverlay('live', 'en', noop), container);
    expect(container.textContent?.trim()).toBe('');
  });

  it('keeps waiting and reconnecting non-blocking', () => {
    for (const status of ['waiting', 'reconnecting'] as ViewStatus[]) {
      render(viewOverlay(status, 'en', noop), container);
      const banner = container.querySelector('.view-banner');
      expect(banner).not.toBeNull();
      expect(banner?.getAttribute('role')).toBe('status');
      expect(container.querySelector('[aria-modal="true"]')).toBeNull();
    }
  });

  it('says the host cannot send a picture instead of blaming the connection', () => {
    for (const [status, fragment] of [
      ['no-capture', 'screen capture'],
      ['no-encoder', 'video encoder'],
    ] as [ViewStatus, string][]) {
      const dismissed = vi.fn();
      render(viewOverlay(status, 'en', dismissed), container);
      const modal = container.querySelector('[role="alertdialog"]');
      expect(modal?.getAttribute('aria-modal')).toBe('true');
      // The distinction that matters: this is not a lost connection.
      expect(container.textContent).toContain(fragment);
      expect(container.textContent).not.toContain('Connection lost');
      container.querySelector('button')?.click();
      expect(dismissed).toHaveBeenCalledOnce();
    }
  });

  it('makes the terminal failure a modal whose only action ends the session', () => {
    const dismissed = vi.fn();
    render(viewOverlay('failed', 'en', dismissed), container);
    const modal = container.querySelector('[role="alertdialog"]');
    expect(modal?.getAttribute('aria-modal')).toBe('true');

    const button = container.querySelector('button');
    expect(button?.autofocus).toBe(true);
    button?.click();
    expect(dismissed).toHaveBeenCalledOnce();
  });

  for (const locale of SUPPORTED_LOCALES) {
    for (const status of [
      'waiting',
      'reconnecting',
      'failed',
      'no-capture',
      'no-encoder',
    ] as ViewStatus[]) {
      it(`has no axe violations (${status}, ${locale})`, async () => {
        render(viewOverlay(status, locale, noop), container);
        const results = await axe.run(container, {
          rules: Object.fromEntries(LAYOUT_DEPENDENT_RULES.map((id) => [id, { enabled: false }])),
        });
        expect(results.violations).toEqual([]);
      });
    }
  }
});
