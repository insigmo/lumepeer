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

import { SUPPORTED_LOCALES } from './i18n';
import {
  decodeViewFrame,
  logicalOfButton,
  logicalOfKey,
  paintFrame,
  POINTER_BUTTON_LOGICAL_BASE,
  suppressContextMenu,
  ViewInput,
  viewOverlay,
  VIEW_RESPONSE_HEADER_BYTES,
  type InputSink,
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
    return { canvas, input: new ViewInput(canvas, sink, container), calls };
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
    canvas.dispatchEvent(new MouseEvent('pointerdown', { button: 1, bubbles: true }));
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

  it('drops keys it cannot map rather than inventing an identifier', () => {
    const { input, calls } = surface();
    input.setEnabled(true);
    container.dispatchEvent(new KeyboardEvent('keydown', { key: 'BrightnessUp', bubbles: true }));
    expect(calls).toEqual([]);
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
