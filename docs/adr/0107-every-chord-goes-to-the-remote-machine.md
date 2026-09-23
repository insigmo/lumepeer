# ADR 0107 — Every chord goes to the remote machine

Status: accepted
Date: 2026-09-23

Answers the report "when VMware on the remote machine has grabbed the mouse
and keyboard, `Ctrl+G` and `Ctrl+Alt` do not get through, and neither do many
other hotkeys; send almost every combination, except the protected ones like
`Ctrl+Alt+Del`". Widens
[ADR 0090](0090-the-keys-a-view-window-never-sees.md), which
`docs/bugs/20-hotkeys-and-vmware-grab.md` forbids doing without an ADR.

## Context

ADR 0090's keyboard grab took a short list — the `Win` keys, `Alt+Tab`,
`Alt+Esc`, `Ctrl+Esc`, `Alt+F4`, `PrintScreen` — and left every other
keystroke to the webview. Reading the code for this report turned up three
ways that split loses a chord:

1. **Anything this machine claims first never reaches the webview.** Not only
   the shell: any program on the operator's machine that registered a global
   hotkey with `RegisterHotKey` gets its chord before a window is told, and
   so does the input-language switcher. No list of "system chords" can name
   them, because they are whatever the operator happens to have installed.
2. **A chord travelled two paths.** `Alt` went through the webview —
   `keydown`, JavaScript, IPC, the actor — and `Tab` through the hook's
   channel, which is much shorter. Nothing ordered the two, so the host could
   be sent `Tab` before the `Alt` it was held under.
3. **The hook read modifiers it had hidden.** It asked `GetAsyncKeyState`
   whether `Win` was down. Inside a `WH_KEYBOARD_LL` callback that state does
   not yet reflect the event being handled, and it never reflects one the hook
   swallowed — and the hook swallowed `Win`. So `Win+D` sent `Win` from the
   hook and a plain 'd' from the webview, which the host typed as a character.

## Decision

### The grab takes every chord, in one stream

`lumepeer_guestkeys::route` answers, per keystroke, one of three things:
**here** (this machine keeps it; the webview forwards it if it is typing),
**there** (sent, and hidden from this machine) or **both** (sent, and let
through here too). While a grab is live:

| Keystroke | Route |
| --- | --- |
| `Ctrl`, `Alt`, `Shift` themselves | both |
| either `Win` | there |
| any key under `Ctrl`, `Alt` or `Win` | there |
| any key under `Shift` that does not type a character (`Shift+Tab`, `Shift+Del`, `Shift+F10`, `Shift+arrow`) | there |
| `PrintScreen` | there |
| everything under exactly `Ctrl+Alt+Shift` — the view window's own chords | here |
| `Ctrl+Alt+Del` (either `Del`), `Win+L` | here |
| a release whose press the grab did not send | here |
| plain and shifted typing | here |

A key the grab sent down stays the remote machine's until it comes up, so a
press and its release never take different paths.

Everything the grab sends goes out of one hook callback, over one channel, in
the order the keyboard produced it. `Ctrl+Alt` — which is all `VMware`'s
release chord is — is now two keystrokes on that one path, and so is every
modifier a chord is held under.

### Why `Ctrl`, `Alt` and `Shift` are sent *and* seen here

Hiding them from this machine would be simpler and would break two things.
The view window recognises its own chords (`Ctrl+Alt+Shift+K` releases the
grab) from `KeyboardEvent.ctrlKey` and friends, which a hidden `Ctrl` never
sets. And this machine's own idea of what is held has to stay true, or a
modifier comes back up here that it never saw go down.

The webview therefore sees them as well and forwards them as it always has.
`input_press` drops that copy while the grab is live for that host
(`KeyboardGrab::already_sent`, `lumepeer_guestkeys::shared_with_this_machine`).
The grab sends their releases unconditionally, even ones whose press predated
it, so the copy it drops can never be the only release the host would have
got.

`Win` stays hidden, as ADR 0090 decided: a `Win` this machine sees opens its
own Start menu on release.

### Modifiers are what the grab sent

The grab no longer asks the OS what is held. It reads its own set of keys it
has sent down (`Held::of_positions`), which is exactly what the host has down
— every modifier pressed while it is live is sent.

### Typing is not a chord

Plain and shifted characters stay on the webview's path and are sent as the
character the operator's own layout makes, exactly as
[ADR 0065](0065-a-chord-is-a-position-on-the-keyboard.md) decided: a Russian
operator on an English host still types Russian. `Shift` travels through the
grab, and a shifted character does not care — the host types it through
`KEYEVENTF_UNICODE`, which ignores `Shift`. A shifted key that is not a
character is on the grab's path for exactly that reason: `Shift+Tab` must not
arrive before its `Shift`.

### The grab steps aside for the window's own fields

Taking every chord means `Ctrl+V` into the chat box, or `Ctrl+C` into the
terminal, would go to the host instead. While one of the view window's own
text fields has the focus — `isLocalTextTarget`, the same test `ViewInput`
already uses — the window says so (`view_keyboard_grab` with `local_field`)
and the grab is released until the focus leaves it.

### A `Shift` with the `E0` prefix has no position

Neither `Shift` key carries the prefix. One that does is a *fake* shift the
keyboard or the OS wraps around a numpad or navigation key to undo `NumLock`;
the host's own OS makes the same ones for the same key. Forwarded, it would
hold `Shift` down over there around an arrow nobody meant to select with.

## Consequences

- `Ctrl+G`, `Ctrl+Alt`, `Ctrl+Alt+<anything>`, `Win+<anything>` and every
  other chord reach the host however this machine is configured, and in the
  order they were pressed.
- While a view window is focused the operator's own global hotkeys, and any
  `Ctrl`/`Alt`/`Win` chord at all, are the remote machine's. The way back is
  unchanged: `Ctrl+Alt+Shift+K`, or clicking another window.
- `Ctrl+Alt+Del` and `Win+L` act on this machine, as they always did — no
  hook can stop them. The host is sent its own `Ctrl+Alt+Del` by the window's
  request (`Ctrl+Alt+Shift+D`, [ADR 0028](0028-remote-ctrl-alt-del-needs-a-privileged-helper.md)).
- Tests (`crates/guestkeys`): `a_grab_sends_every_chord`,
  `ctrl_alt_and_shift_are_sent_and_still_seen_here`,
  `typing_is_left_to_this_machine`,
  `a_grab_never_takes_the_view_windows_own_chords`,
  `the_protected_chords_stay_here`, `a_key_sent_down_is_sent_up`,
  `the_held_modifiers_are_the_ones_sent`, and the fake shifts in
  `a_key_with_no_known_position_is_not_given_one`. In the desktop app:
  `nothing_is_grabbed_while_the_window_types_into_its_own_field`.

## Still open

- **Whether `VMware` acts on an injected `Ctrl+Alt` at all.** This ADR makes
  sure the host is *sent* it, in order, by position, with scan codes. What the
  host does next is `SendInput`, and nobody has yet watched a `VMware`
  Workstation window with a grabbed VM receive it — the same open check as
  ADR 0065 and `docs/bugs/20-hotkeys-and-vmware-grab.md` task 3.1. If it
  ignores injected keystrokes (`LLKHF_INJECTED`) or reads the keyboard through
  its enhanced keyboard driver, no guest-side change can reach it; that is a
  host-side decision and its own ADR.
- **Plain typing into a grabbed VM.** A typed character still reaches the host
  as `KEYEVENTF_UNICODE`, which carries no scan code, and `VMware` forwards
  scan codes. Chords go by position and are not affected; ordinary text typed
  into a VM that holds the grab may be. Unverified, for the same reason.
- Like ADR 0090's hook, the callback itself is verified by hand, not by a
  test: a synthesized keystroke carries `LLKHF_INJECTED`, which the hook
  ignores on purpose, so no test on this machine can press a key it will see.
- A press and release of `Alt` with the chord key hidden reads to this machine
  as `Alt` pressed alone, which a Win32 window may answer by entering its
  menu. ADR 0090's `Alt+Tab` already produced exactly this sequence and no
  report has been traced to it; if one ever is, the remedy other hook-based
  tools use is to inject an unassigned "mask" key while `Alt` is held.
