# ADR 0090 — The keys a view window never sees

Status: accepted
Date: 2026-09-18

Answers the report "hotkeys are not forwarded to the machine; copy, paste and
some random combinations do not work", which
[ADR 0065](0065-a-chord-is-a-position-on-the-keyboard.md) did not close.

## Context

ADR 0065 fixed what the *host* does with a chord: press it by position, so
`Ctrl+C` arrives as `VK_CONTROL` plus `'C'` rather than as the character 'c'
through `KEYEVENTF_UNICODE`. That was necessary and it was not sufficient,
because it assumed the chord reached the guest's own process at all. Two whole
classes of keystroke never do, and both were invisible from the host's side —
the host simply received nothing.

**The webview eats some of them.** `WebView2` ships with
`AreBrowserAcceleratorKeysEnabled` set, and Tauri exposes no way to clear it
(`wry` has `with_browser_accelerator_keys`; `tauri-runtime-wry` does not plumb
it). So the window that draws a remote screen answered a browser's idea of
every one of these itself, and the host saw none of them:

| Chord | What it did locally |
| --- | --- |
| `Ctrl+R`, `F5` | reloaded the page the remote picture is drawn on |
| `Ctrl+P` | offered to print that page |
| `Ctrl+F`, `Ctrl+G` | opened a find bar over the remote screen |
| `Ctrl+U` | offered its source |
| `Ctrl+S` | offered to save it |
| `Ctrl+0`, `Ctrl+±` | zoomed the page |
| `Alt+Left`, `Alt+Right` | navigated its history |
| `F12`, `Ctrl+Shift+I/J/C` | opened the developer tools |

**The OS eats the rest.** `Win+D`, `Win+E`, `Win+L`, `Win+R`, `Win+Tab`,
`Win+arrow`, `Alt+Tab`, `Alt+Esc`, `Ctrl+Esc`, `Ctrl+Shift+Esc`, `Alt+F4` and
`PrintScreen` are claimed by the shell before any window is told, so no amount
of `preventDefault` reaches them. A remote machine could not be sent its own
Start menu.

Neither is a protocol problem and neither has anything to do with grants: the
host's `SessionManager::authorize_input` never saw an event to refuse.

## Decision

### The view window gives its browser chords back

One COM call, on each view window as it opens:
`ICoreWebView2Settings3::SetAreBrowserAcceleratorKeysEnabled(false)`. It
removes only the *browser-specific* accelerators. `Ctrl+C`, `Ctrl+V`,
`Ctrl+X`, `Ctrl+A`, `Ctrl+Z` and the arrows are not a browser's, keep reaching
the page, and keep being forwarded by `ViewInput` exactly as before.

The main window is untouched. Its keystrokes belong to it.

### A keyboard grab, for the chords no window is told about

A `WH_KEYBOARD_LL` hook, live **while a view window that holds `input` is
focused and the operator has not released it**, which claims the list in
`claimed_by_the_remote_machine` and passes everything else through
untouched:

- either `Win` key, and anything held with one;
- `Alt+Tab`, `Alt+Shift+Tab`, `Alt+Esc`;
- `Ctrl+Esc` and `Ctrl+Shift+Esc`;
- `Alt+F4`;
- `PrintScreen`.

Everything not on that list keeps travelling the ordinary way — the webview
sees it, `ViewInput` forwards it — so a bug in the hook cannot cost anyone
their keyboard beyond those chords. `Ctrl+Alt+Del` is not on the list and
could not be: the Secure Attention Sequence is unhookable by design, which is
what makes it secure, and the window asks for it as a message instead
([ADR 0028](0028-remote-ctrl-alt-del-needs-a-privileged-helper.md)).

A grabbed keystroke is sent with `logical` of 0 — which
`lumepeer_core::protocol::names_a_key` reads as "no character meaning" — and
the evdev position of the key. The host therefore presses it **by position**,
which is ADR 0065's own rule, and the generic-modifier trap that ADR
0065 describes cannot arise because no grabbed key ever travels as a
character.

### Three bounds on the grab, each for its own reason

- **Focus.** A `WH_KEYBOARD_LL` hook is global: it sees every keystroke on the
  desktop. A grab that outlived the window's focus would send the operator's
  own `Win+E` to somebody else's machine. Focus comes from Tauri's window
  events rather than from `focus`/`blur` inside the webview, because a webview
  that has not finished loading reports neither.
- **The `input` grant.** A view-only session has no use for a keystroke. The
  host re-checks per event regardless (§2.3); this only keeps the local
  keyboard from being taken for nothing.
- **`Ctrl+Alt+Shift+K`.** While the grab is on, the operator cannot `Alt+Tab`
  away from the window — that is the point, and it is also a trap without a
  way out. The view window's own chords all start `Ctrl+Alt+Shift`, which the
  grab never claims, so this one is always available; it is in the toolbar's
  hotkey list like every other, because a hotkey nobody can see is
  indistinguishable from a bug.

**On by default.** The report this answers is that a remote machine could not
be sent its own hotkeys, and a fix nobody turns on does not answer it.

### It lives in a crate of its own

`crates/guestkeys`. The desktop app is `#![forbid(unsafe_code)]` — a `forbid`
no inner `allow` can lift — and neither `SetWindowsHookExW` nor a COM property
setter has a safe binding. Same reasoning, and the same shape, as
`lumepeer-terminal` ([ADR 0079](0079-a-terminal-is-its-own-grant-and-never-the-clients-privileges.md)).

Two facts about `WH_KEYBOARD_LL` shape that crate:

- The callback runs on the installing thread, which must be pumping messages,
  so the crate owns a thread with a bare `GetMessageW` loop. Not the Tauri
  main thread: that one also lays out and paints a webview.
- The callback has a deadline. Windows silently removes a hook whose callback
  overruns `LowLevelHooksTimeout` (300 ms by default), so nothing in it may
  block: it queues onto an unbounded channel and a task does the awaiting.

The grab releases, over there, every key it is still holding when it is
dropped — the same rule, for the same reason, as `ViewInput::releaseHeld`.

## Consequences

- The chords in both tables above now reach the host. `Ctrl+R` no longer
  reloads the remote picture.
- While a view window is focused, the operator's own `Win` and `Alt+Tab` are
  the remote machine's. That is a real change to their desktop and it is why
  the release chord exists and is documented.
- Off Windows both halves are a documented no-op. `WebKitGTK` and `WKWebView`
  do not claim the set `WebView2` does, and neither X11 nor Wayland hands a
  client the compositor's own chords. Each is its own task and neither is
  pretended to be solved.
- `evdev_of_virtual_key` is the exact inverse of `physical_key` in
  `crates/media/src/capture/windows.rs`. They are each other's mirror, and a
  change to one is a change to both; a test asserts the round trip for every
  part of the keyboard.
- Tests (`crates/guestkeys`):
  `a_grab_claims_the_system_chords_and_nothing_else`,
  `a_grab_never_takes_the_view_windows_own_chords`,
  `the_positions_are_the_ones_the_host_reads_them_back_as`,
  `a_key_with_no_known_position_is_not_given_one`,
  `the_modifier_bits_are_the_ones_the_host_reads`. In the desktop app:
  `the_grab_is_on_by_default`,
  `nothing_is_grabbed_without_a_focused_window_that_holds_input`.

## Still open

- The hook itself is Win32 and is verified by hand, like every other
  `SendInput`-adjacent path in this workspace
  ([ADR 0057](0057-lumepeer-takes-full-local-control.md)): what is unit-tested
  is which chords are claimed and where each key sits, not that Windows
  delivers the callback. Belongs in `docs/release-checklist.md`.
- The grab has no indicator. Its state is observable only by pressing the
  chord and noticing that `Alt+Tab` works again. A toolbar button would be
  better and is left out of this change deliberately, to keep it to the
  keyboard.
- `Ctrl+Alt+Del` still goes through ADR 0028's request rather than the
  keyboard, and always will.
