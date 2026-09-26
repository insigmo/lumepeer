# ADR 0115 — Typing goes out as the host's own key for the character

Status: accepted
Date: 2026-09-26

Amends [ADR 0065](0065-a-chord-is-a-position-on-the-keyboard.md)'s "otherwise,
type the character". Follows [ADR 0114](0114-the-desktop-is-injected-by-a-system-worker.md),
which is what let input reach a `VMware` guest at all.

## Context

Once ADR 0114 put the injector above the `VMware` window, the operator
reported: chords work, but typing inside the VM gives the wrong keys — '1'
comes out as 'n', '2' as 'm', and letters type nothing.

That is `KEYEVENTF_UNICODE` read as a scan code. Typing without Ctrl, Alt or
Meta went out as `KEYEVENTF_UNICODE`, which puts the **character** in
`wScan`. An ordinary window never looks at that field for such a press, but a
`VMware` window reads the scan code of every keystroke and hands it to the
guest OS:

| Typed | `wScan` sent | Set-1 key at that code |
|---|---|---|
| `1` | 0x31 | N |
| `2` | 0x32 | M |
| space | 0x20 | D |
| `a`..`z` | 0x61..0x7A | none |

ADR 0065 said a Unicode press "carries no scan code". It carries the wrong one,
and every scan-code reader (a VM, an RDP client, a game reading `DirectInput`)
takes it as a key.

## Decision

**A typed character goes out as the scan code of the key on the host's layout
that types exactly that character at the guest's shift level. Only a character
with no such key goes out as `KEYEVENTF_UNICODE`.**

- **Which layout.** The layout of the thread that owns the foreground window.
  Layouts are per thread, and the injector's own layout says nothing about the
  window being typed into. This matters more for ADR 0114's `LocalSystem`
  worker than anywhere else.
- **Which key.** There are two candidates, and a candidate counts only if
  `ToUnicodeEx` confirms the host types exactly that character with it: the
  key under the guest's finger, then the key `VkKeyScanExW` finds for the
  character. When both layouts agree, both candidates are the same key. When
  they differ, the second one finds the host's own key; a Russian guest's '.'
  is under the US '/' key.
- **Shift** comes from the guest's `modifiers`, as in the X11 injector's
  `keycode_for`. The host already has Shift down, because the guest forwarded
  the Shift press.
- **`CapsLock` is accepted either way.** A thread that never receives keyboard
  input cannot read the host's toggle reliably. The guest's `CapsLock` presses
  are forwarded, so the two normally agree.
- **A numpad digit is typed on the digit row.** What the numpad key presses
  depends on the host's `NumLock`, which can differ from the guest's. The
  numpad operators are not `NumLock`-dependent and keep their own keys.
- **A key comes back up the way it went down.** The injector remembers every
  guest key it pressed by scan code, keyed by the guest's evdev code. The
  release and every auto-repeat go out as that same key, whatever character
  the guest reports by then. Shift pressed mid-hold turns a Russian guest's
  '.' into ',', which a US host has on another key. Without this, the first
  key would stay down, and inside a VM a stuck key auto-repeats. A typed key
  that went down as a character comes back up as one.

Dead keys, AltGr characters and characters outside the BMP fail the
`ToUnicodeEx` check, so they stay characters.

## Consequences

- Typing reaches a `VMware` guest whenever the host's layout can type what the
  operator typed. That covers digits, Latin letters, space and punctuation
  whenever both machines have the same layout active.
- In an ordinary window the result is unchanged, except in the edge cases
  below. Keystrokes now carry real virtual keys, as a physical keyboard's do.
- **Out of reach:** a character the host's active layout has no key for. For
  example, Cyrillic with the host on English. It still goes as
  `KEYEVENTF_UNICODE`: correct in an ordinary window, garbage inside a VM. The
  fix is to switch the host to the matching layout. The input method is the
  host's, the same as for a physical keyboard plugged into it.
- **Out-of-sync `CapsLock`** — the host's lock on while the guest's is off —
  now types letters in the other case in ordinary windows too. Before this
  ADR, only chords and VMs were affected. Pressing `CapsLock` in the view
  toggles both machines, so the difference remains. To clear it, toggle the
  lock once while the view is not focused.
- The secure-desktop worker (`crates/service/src/secure_desktop_input.rs`) still
  types by character. `Winlogon` holds no VM, and that crate does not depend
  on this one.
- **Not yet verified against `VMware`.** The mapping is unit-tested against
  the US layout, including the reported '1' → 0x02 case. A live check inside
  the guest on `beta` is still owed.
