// Client hotkeys of the remote-view window (§11).
//
// The rule this file exists for: this window forwards keystrokes to another
// machine, so every chord it keeps is a key the operator loses over there.
// Only an exact match may be taken, and only the matched chord may be
// consumed — everything else has to travel on byte-for-byte unchanged.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { HOTKEYS, HOTKEY_PREFIX, hotkeyLabel, installHotkeys, matchHotkey } from './view-hotkeys';
import { ViewInput, type InputSink } from './view-window';

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement('div');
  document.body.appendChild(container);
});

afterEach(() => {
  container.remove();
});

function chord(code: string, over: Partial<KeyboardEventInit> = {}): KeyboardEvent {
  return new KeyboardEvent('keydown', {
    code,
    key: code.startsWith('Key') ? code.slice(3).toLowerCase() : code,
    ctrlKey: true,
    altKey: true,
    shiftKey: true,
    bubbles: true,
    cancelable: true,
    ...over,
  });
}

describe('view hotkeys: matching', () => {
  it('matches every documented chord', () => {
    for (const entry of HOTKEYS) {
      expect(matchHotkey(chord(entry.code))).toBe(entry.action);
    }
  });

  it('takes nothing without the full prefix', () => {
    for (const missing of [{ ctrlKey: false }, { altKey: false }, { shiftKey: false }]) {
      expect(matchHotkey(chord('KeyF', missing))).toBeNull();
    }
  });

  it('is an exact match, so a superset chord belongs to the remote machine', () => {
    expect(matchHotkey(chord('KeyF', { metaKey: true }))).toBeNull();
  });

  it('leaves Escape alone: it is the worst key to take from a remote desktop', () => {
    expect(matchHotkey(chord('Escape'))).toBeNull();
    expect(HOTKEYS.some((entry) => entry.code === 'Escape')).toBe(false);
  });

  it('names each chord the same way the help popover does', () => {
    expect(hotkeyLabel('KeyF')).toBe(`${HOTKEY_PREFIX}+F`);
    expect(hotkeyLabel('Digit0')).toBe(`${HOTKEY_PREFIX}+0`);
  });
});

describe('view hotkeys: installation', () => {
  it('runs the handler and consumes only the chord it matched', () => {
    const fullscreen = vi.fn();
    const stop = installHotkeys(container, { 'toggle-fullscreen': fullscreen });
    try {
      const matched = chord('KeyF');
      container.dispatchEvent(matched);
      expect(fullscreen).toHaveBeenCalledOnce();
      expect(matched.defaultPrevented).toBe(true);

      const passed = chord('KeyF', { metaKey: true });
      container.dispatchEvent(passed);
      expect(fullscreen).toHaveBeenCalledOnce();
      expect(passed.defaultPrevented).toBe(false);

      const ordinary = new KeyboardEvent('keydown', {
        code: 'KeyF',
        key: 'f',
        bubbles: true,
        cancelable: true,
      });
      container.dispatchEvent(ordinary);
      expect(ordinary.defaultPrevented).toBe(false);
    } finally {
      stop();
    }
  });

  it('takes nothing while the operator is typing into a field of this window', () => {
    // Same document-scoped trap the forwarder has: the chat box is inside this
    // window, so a chord typed there would otherwise fire the action and be
    // eaten before the field ever saw it.
    const chat = vi.fn();
    const field = document.createElement('input');
    container.appendChild(field);
    const stop = installHotkeys(container, { 'toggle-chat': chat });
    try {
      const typed = chord('KeyC');
      field.dispatchEvent(typed);
      expect(chat).not.toHaveBeenCalled();
      expect(typed.defaultPrevented).toBe(false);

      const overPicture = chord('KeyC');
      container.dispatchEvent(overPicture);
      expect(chat).toHaveBeenCalledOnce();
      expect(overPicture.defaultPrevented).toBe(true);
    } finally {
      stop();
    }
  });

  it('leaves an action it has no handler for untouched', () => {
    const stop = installHotkeys(container, {});
    try {
      const event = chord('KeyM');
      container.dispatchEvent(event);
      expect(event.defaultPrevented).toBe(false);
    } finally {
      stop();
    }
  });

  it('stops taking anything once removed', () => {
    const fullscreen = vi.fn();
    installHotkeys(container, { 'toggle-fullscreen': fullscreen })();
    container.dispatchEvent(chord('KeyF'));
    expect(fullscreen).not.toHaveBeenCalled();
  });

  it('does not also send the chord to the remote machine', () => {
    // The whole reason `installHotkeys` runs in the capture phase: half a
    // chord arriving on the host is worse than no hotkey at all.
    const calls: string[] = [];
    const sink: InputSink = {
      pointerMove: () => {},
      press: (logical, _scancode, _modifiers, pressed) => calls.push(`${logical} ${pressed}`),
      wheel: () => {},
    };
    const canvas = document.createElement('canvas');
    container.appendChild(canvas);
    const input = new ViewInput(canvas, sink, container);
    input.setEnabled(true);
    const stop = installHotkeys(container, { 'toggle-fullscreen': () => {} });
    try {
      container.dispatchEvent(chord('KeyF'));
      expect(calls).toEqual([]);

      // And an ordinary key still reaches the host.
      container.dispatchEvent(
        new KeyboardEvent('keydown', { code: 'KeyA', key: 'a', bubbles: true, cancelable: true }),
      );
      expect(calls).toEqual(['97 true']);
    } finally {
      stop();
      input.setEnabled(false);
    }
  });
});
