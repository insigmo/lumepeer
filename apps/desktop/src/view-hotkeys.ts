// Client-side hotkeys of the remote-view window (design doc §11).
//
// This window forwards keystrokes to another machine. Every hotkey it keeps
// for itself is a key the operator can no longer type over there, which makes
// the choice of chord a correctness question rather than a matter of taste:
//
// - The prefix is `Ctrl+Alt+Shift`, a combination no ordinary desktop work
//   uses, so nothing an operator is trying to do on the host collides with it.
//   On a Mac it is `⌃⌥⌘` (Control+Option+Command): a Mac keyboard has no Alt,
//   an app's own chords there go through Command, and three modifiers keep the
//   same rarity. It is written the way macOS menus write it, `⌃⌥⌘F` (ADR 0158).
// - Matching is exact. A chord with the one modifier the prefix leaves out —
//   `Meta`, or `Shift` on a Mac — also held is *not* a match and travels on
//   untouched; matching loosely would silently eat a superset of what is
//   documented.
// - `Escape` is deliberately not a hotkey, even for leaving full screen. It is
//   one of the most-used keys on any remote machine, and taking it would be
//   the single worst key to lose.
// - Only the matched chord is consumed. `preventDefault` marks it so
//   `ViewInput` knows not to forward it as well, and everything else is left
//   exactly as it arrived.
// - Nothing matches while the operator is typing into one of this window's own
//   fields. The listener sits on the document, so a chord typed into the chat
//   box would otherwise fire the window's action *and* be eaten before the
//   field saw it — the same document-scoped trap `ViewInput` has, which is why
//   both read the same {@link isLocalTextTarget}.
//
// A hotkey nobody can see is indistinguishable from a bug, so the same table
// this module matches against is what the toolbar's help popover lists. That
// matters most for `toggle-keyboard-grab` (ADR 0090): while the grab is on,
// `Alt+Tab` belongs to the remote machine, so this chord is the operator's
// way back to their own window switcher — and it has to be discoverable.

import { IS_MAC } from './platform';
import { isLocalTextTarget } from './view-window';

/** One thing the operator can ask this window — not the host — to do. */
export type HotkeyAction =
  | 'toggle-fullscreen'
  | 'cycle-display-mode'
  | 'reset-view'
  | 'toggle-chat'
  | 'send-cad'
  | 'toggle-toolbar'
  | 'toggle-keyboard-grab'
  | 'toggle-stats';

/** The chords, as physical key codes so a non-QWERTY layout still matches. */
export const HOTKEYS: readonly { code: string; action: HotkeyAction }[] = [
  { code: 'KeyF', action: 'toggle-fullscreen' },
  { code: 'KeyM', action: 'cycle-display-mode' },
  { code: 'Digit0', action: 'reset-view' },
  { code: 'KeyC', action: 'toggle-chat' },
  { code: 'KeyD', action: 'send-cad' },
  { code: 'KeyT', action: 'toggle-toolbar' },
  { code: 'KeyK', action: 'toggle-keyboard-grab' },
  { code: 'KeyS', action: 'toggle-stats' },
];

/** How the prefix is written wherever the chords are shown to a person. */
export function hotkeyPrefix(mac: boolean = IS_MAC): string {
  return mac ? '⌃⌥⌘' : 'Ctrl+Alt+Shift+';
}

/** The printable name of one chord, for the help popover. */
export function hotkeyLabel(code: string, mac: boolean = IS_MAC): string {
  const key = code.startsWith('Key')
    ? code.slice('Key'.length)
    : code.startsWith('Digit')
      ? code.slice('Digit'.length)
      : code;
  return `${hotkeyPrefix(mac)}${key}`;
}

/**
 * The action this event asks for, or `null` when it is not one of ours.
 *
 * Exact match only: all three prefix modifiers held, the fourth not held,
 * and a code in the table. Anything else belongs to the remote machine.
 */
export function matchHotkey(event: KeyboardEvent, mac: boolean = IS_MAC): HotkeyAction | null {
  const prefixHeld = mac
    ? event.ctrlKey && event.altKey && event.metaKey && !event.shiftKey
    : event.ctrlKey && event.altKey && event.shiftKey && !event.metaKey;
  if (!prefixHeld) {
    return null;
  }
  return HOTKEYS.find((entry) => entry.code === event.code)?.action ?? null;
}

/** What the window does when a chord matches. */
export type HotkeyHandlers = Partial<Record<HotkeyAction, () => void>>;

/**
 * Installs the hotkey listener on `target`, ahead of the input forwarder.
 *
 * Registered in the capture phase so a matched chord is marked before
 * `ViewInput`'s own bubbling listener sees it: the forwarder then skips it
 * because it is already `defaultPrevented`. Returns a teardown.
 */
export function installHotkeys(target: EventTarget, handlers: HotkeyHandlers): () => void {
  const onKeyDown = (event: Event): void => {
    if (isLocalTextTarget(event.target)) {
      return;
    }
    const action = matchHotkey(event as KeyboardEvent);
    if (!action) {
      return;
    }
    const handler = handlers[action];
    if (!handler) {
      return;
    }
    event.preventDefault();
    handler();
  };
  target.addEventListener('keydown', onKeyDown, true);
  return () => target.removeEventListener('keydown', onKeyDown, true);
}
