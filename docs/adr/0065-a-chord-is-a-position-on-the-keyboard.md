# ADR 0065 — A chord is a position on the keyboard, and a grabbed pointer is driven by deltas

Status: accepted
Date: 2026-09-07

Extends §11's input path. See `docs/bugs/17-remote-hotkeys.md` §2 and §3.

## Context

Two symptoms, one cause underneath and one alongside it.

**Ctrl+C copied nothing.** A guest webview reports `KeyboardEvent.key`, which
is the *character* its own layout puts on the key. The host, given a
character it has no name for, typed it: `KEYEVENTF_UNICODE` on Windows, a
keysym lookup on X11. `KEYEVENTF_UNICODE` carries no virtual key at all, so
what the application received was a stray character arriving while Ctrl
happened to be held. No accelerator fires on that. Ctrl+C did not work
intermittently; it never worked.

On a non-Latin layout it was worse than not working. A guest with a Russian
layout reports the Cyrillic `с` under Ctrl, and a US-layout host cannot
produce that character at all — which is why the failure read to the operator
as "some random combinations don't work". Which combinations depended on the
layouts at each end, not on the chord.

Two smaller gaps sat next to it. `ContextMenu`, `PrintScreen`, `ScrollLock`,
`Pause`, `NumLock` and the entire numeric keypad had no identifier on the
wire and went nowhere. And a chord interrupted by Alt+Tab delivered its press
and never its release — the window stops receiving key events the moment it
loses focus — so the host went on believing Ctrl was down and every later
keystroke was silently a chord.

**Nothing could be controlled inside a VMware virtual machine.** While a
VMware Workstation window holds the pointer grab, the host cursor is pinned
and hidden and *every* absolute move lands nowhere — injected or from the
physical mouse alike. Measured on host `beta`, 2026-08-25: `SendInput`
returned `sent=1`, `SetCursorPos` returned `False`, and the cursor did not
move. What drives the VM in that state is relative motion, read below the
cursor, which is why a physical mouse still steers it and this injector did
not. It is not UIPI: running lumepeer elevated changed nothing, twice.

## Decisions

### The character and the position both travel, and which one is pressed
### depends on the modifiers

The guest fills in `InputEventPayload::scancode` with the evdev code of
`KeyboardEvent.code`. evdev because that is what the field has always meant
on this wire — the X11 injector reads it as `keycode - 8` — so one physical
encoding crosses the network and each host platform translates from it.
`code` is a position, and a position does not move when either side switches
layouts.

The rule is one predicate, `lumepeer_core::protocol::is_chord`, read by every
injector:

- **Under Ctrl, Alt or Meta, press by position.** The keystroke is a command,
  and a command is addressed to a virtual key.
- **Otherwise, type the character.** What the operator meant then appears on
  the host whatever layout the host is set to, which is the property the
  Unicode path was always there for and which is worth keeping.
- **Shift is not a chord modifier.** It selects a character rather than
  commanding with one, and a shifted key sent by position would type whatever
  the host's layout has there instead of what the operator saw themselves
  type.

AltGr is deliberately not special-cased. On Windows it *is* Ctrl+Alt, so a
guest typing AltGr+key takes the physical path and the host's own layout
produces its own AltGr character for that position — which is exactly what a
physical keyboard plugged into the host would have done.

**A key with a position and no character** — `PrintScreen`, a dead key, a
`code` the guest's layout leaves unlabelled — is pressed by position with no
modifier required. The guest sends `logical: 0` for it, which every injector
reads as "no character", and five more named keys were added for the ones a
person actually reaches for.

**The Windows injector fills in `wScan` as well as `wVk`**, from
`MapVirtualKeyW` against the host's own current layout, and carries
`KEYEVENTF_EXTENDEDKEY` for the keys that exist twice on a keyboard — the
right-hand modifiers, the navigation cluster against the numpad, the numpad's
own Enter and divide. An ordinary window needs only the virtual key. Anything
reading the keyboard *below* the window needs the scan code, and that is the
other half of this ADR.

**The guest lets go of what it is holding** when the window loses focus and
when the input grant drops mid-press, tracked by `code` so a release matches
its press even when Shift changed the character underneath it.

### The pointer's mechanism is chosen by measurement

`WindowsInjector` aims absolutely, reads `GetCursorPos` back, and after
`GRAB_MISSES_BEFORE_RELATIVE` moves that did not land where they were aimed,
sends deltas instead. One move in `GRAB_PROBE_EVERY` is spent on an absolute
probe, which is how the session returns to absolute motion on its own once
the operator releases the grab with Ctrl+Alt.

More than one miss before switching, because a single miss is also what an
ordinary race with the host's own mouse looks like. A desktop that will not
report the cursor at all changes nothing either way.

The decision lives in `Grab`, a type with no Win32 in it, so the state
machine that decides whether an operator can drive a virtual machine is
covered by tests rather than by a desktop nobody has.

The keyboard half of the same symptom is fixed by the scan codes above:
VMware takes the keyboard through a low-level hook and forwards scan codes
into the guest OS, and a `KEYEVENTF_UNICODE` press carries none.

## Consequences

Chords work, including on a guest whose layout the host has never heard of,
and plain typing keeps the layout independence it had. The two are separated
by the one thing that actually distinguishes them, which is whether a
modifier is commanding or selecting.

A relative move is subject to the host's pointer acceleration in a way an
absolute one is not. That does not reach the case this is for: anything that
grabs the pointer reads raw input, which is pre-ballistics. It would show up
if relative mode were ever entered against an ordinary desktop, which is what
the probe and the miss threshold exist to prevent.

**The macOS injector does not get the physical path.** It needs an
evdev-to-`CGKeyCode` table and a Mac to check it against, and this change had
neither; it still types the character and now refuses a private-use logical
rather than typing an unassigned code point at whatever has focus. Chords
towards a macOS host remain broken, in the way they were already broken.

**The VMware half is not verified against VMware.** The grab state machine is
tested; that deltas actually reach the guest OS through VMware's own input
path can only be confirmed on a host running VMware Workstation, and that
check is still owed.

`crates/media`'s `capture-windows` feature now pulls in
`windows/Win32_UI_WindowsAndMessaging` for `GetCursorPos` and
`GetSystemMetrics`. The default build still needs no platform SDK.
