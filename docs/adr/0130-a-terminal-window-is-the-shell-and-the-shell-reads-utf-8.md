# ADR 0130 — A terminal window is the shell, and the shell reads UTF-8

Status: accepted
Date: 2026-09-29

Amends decision 4 of
[ADR 0101](0101-a-terminal-session-is-the-same-session-with-no-media-connection.md)
and the shell start of
[ADR 0079](0079-a-terminal-is-its-own-grant-and-never-the-clients-privileges.md).

## Report

Two things, seen together in one terminal window from Windows to a Mac:

1. The floating toolbar still sat in the window, over the shell's own lines.
2. Russian typed at the Mac's `zsh` came out as `<008b>в<0084>а<0081>…`. The
   request was to say "this language is not supported" instead of printing
   codes.

## Decision

### 1. A terminal window has no toolbar

`view.ts` no longer mounts the toolbar when the window was opened with
`terminal=1`. ADR 0101 had already cut it down to chat, files and the terminal
button; what was left was the terminal button pointing at the window itself,
and two panels that sat on top of the shell. The shell is the whole window,
and its own Close button ends the session as before.

The toolbar's `terminalOnly` hook and the branches it drove are gone with it:
the toolbar is only ever mounted in a window with a picture.

The chat hotkey still opens the chat panel. Nothing else in the terminal
window changes.

### 2. The host's shell gets a UTF-8 `LC_CTYPE` when it has none

The Mac supports Russian. What the shell could not read was UTF-8, because it
ran in the C locale. A Lumepeer started from the Dock, Finder or a launch
agent inherits no `LANG` (Terminal.app sets one for its own shells, and
nothing sets one for ours), so `zsh` took each byte of a Cyrillic letter as a
character of its own. It drew the lead byte and printed the unprintable second
byte as `<0084>`. Reproduced on the Mac with no locale in the environment:
`echo привет` echoed as `п�<0080>иве�<0082>`, and `locale charmap` said
`US-ASCII`.

`crates/terminal/src/unix.rs::shell_command` now adds `LC_CTYPE` to the
inherited environment when the effective character locale is not UTF-8:
`en_US.UTF-8` on macOS and `C.UTF-8` elsewhere. The effective locale resolves
`LC_ALL`, then `LC_CTYPE`, then `LANG`, and an empty variable counts as unset.
Only the character category changes. Messages, dates and sorting stay what the
host had. A set `LC_ALL` is left alone, because it overrides every category
and only the machine's owner puts it there.

Windows is not touched: ConPTY already speaks UTF-8 in both directions.

### 3. No "language not supported" notice

The garbage came from our own shell start, not from a language the OS lacks.
With the fix, a notice would have nothing to fire on. The only case left is a
host whose owner pinned `LC_ALL` to a non-UTF-8 locale. That is rare, and
detecting it would take a new signal from host to guest for an owner's
deliberate choice.

## Consequences

- A host has to run a build with this fix. An older Mac or Linux host still
  starts its shell in the C locale, and the guest cannot correct that.
- `the_shell_speaks_utf8` proves the fix only when the tests run with no UTF-8
  locale of their own (`env -u LANG -u LC_ALL -u LC_CTYPE cargo test`). On the
  Mac it failed with the fix removed (`US-ASCII`) and passed with it.
