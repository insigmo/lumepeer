# ADR 0132 — The terminal fills its window, draws in colour, and copies and pastes

Status: accepted
Date: 2026-09-29

Amends the shell start of
[ADR 0079](0079-a-terminal-is-its-own-grant-and-never-the-clients-privileges.md).
Finds that [ADR 0081](0081-the-terminal-emulator-writes-its-own-stylesheet.md)
does not take effect in a release build (section 5).

## Report

The terminal window was almost all one colour. The first request was to
highlight it, so that it looks good and makes clear what is going on. A
screenshot of a Windows guest on the `debian` host then showed four more
things: the shell was 80 columns wide in a full-screen window, a right click
did nothing, nothing could be copied or pasted, and there was no cursor.

The causes:

1. The host's shell was not told what terminal it was drawn on. `portable-pty`
   adds no `TERM`, and a Lumepeer started from the Dock, a desktop menu or a
   launch agent has none to pass on. With no `TERM`, `git`, `grep`, `ls` and
   Debian's `.bashrc` prompt all take the terminal for a dumb one and print no
   colour at all, and `readline` falls back to scrolling a long line sideways:
   the screenshot's `<aaaa…` line. A Lumepeer started from another terminal
   passed on *that* terminal's `TERM`, which describes the wrong emulator.
2. The guest's `xterm.js` ran on its defaults: the stock ANSI palette on pure
   black, and Courier.
3. The lines the window writes itself ("Shell ended", a refusal) looked exactly
   like the shell's output.
4. Nothing sized the emulator. It stayed at its default 80×24 whatever the
   window was, and `TerminalControls.refit` had no caller.
5. The view window suppresses the webview's own context menu everywhere,
   because over the picture a right click belongs to the host. Over the shell
   that left nothing. `Ctrl+C` and `Ctrl+V` went to the shell as `^C` and `^V`.
6. The missing cursor, the proportional font and the missing colours in the
   screenshot are one more cause, and it is not in this ADR's code: see
   section 5.

## Decision

### 1. The shell is told it is drawn on `xterm.js`

`crates/terminal/src/unix.rs::shell_command` sets `TERM=xterm-256color` and
`COLORTERM=truecolor` on the shell it starts, overriding whatever the process
inherited. Every byte the shell writes is drawn by `xterm.js` in the guest's
window, so that is the terminal, whatever launched the host. `xterm-256color`
is what `xterm.js` implements, and its terminfo entry is on every macOS and in
Debian's `ncurses-base`. `COLORTERM` is where programs look for 24-bit colour,
which terminfo has no standard way to say.

Nothing else in the environment changes. The prompt, `ls` and `git` now colour
what they print the way the host's owner configured them in their own
terminal. A stock macOS `zsh` prompt stays plain, as it is in Terminal.app.

Windows is not touched. ConPTY translates the console's colours itself, and Git
for Windows sets its own `TERM` when it finds none.

### 2. The emulator uses the app's dark palette and monospace font

`TERMINAL_THEME` in `apps/desktop/src/terminal.ts` maps the app's dark tokens
onto the sixteen ANSI colours. Every colour except `black` reads at 4.5:1 or
better on the background. The font is the app's `--font-mono` stack at 13 px,
and the cursor is a blinking bar. The palette is dark whatever the app's theme
is, because the view window always is. The panel's own background changes from
black to the emulator's background, so the chrome and the shell read as one
surface.

### 3. The window's own lines, and its state, are coloured

`TerminalScreen.writeLine` takes a tone. "Shell ended" is a `note`, drawn grey
and italic. A refusal is an `error`, drawn bold red. Both reset attributes
before and after the line, so neither inherits the shell's colour nor leaks
its own into the next prompt. The chrome's state carries `data-status`, and a
dot beside the words shows it: amber and pulsing while the host is being asked,
green while the shell runs, red when the host refused, grey once it is over.
The words stay the whole message; the dot adds nothing a screen reader misses.

### 4. The shell is the size of the window

`@xterm/addon-fit` (0.11.0, the release paired with `xterm.js` 6.0.0) sizes
the grid to the element the emulator is in, once at mount and again whenever
a `ResizeObserver` sees that element change, at most once a frame. Each fit
that changes the grid reaches the host through the existing `onResize` →
`terminal_resize` path. When the host's `opened` arrives, the current size is
sent once more, because the window may have changed size while the host was
answering and there was no shell to tell.

A fit is skipped when the addon proposes its floor of 2×1, which is what it
proposes when there is no room at all: a minimized window, or one not yet
shown. A shell handed 2×1 redraws its prompt two characters to a line into the
scrollback, and that stays after the window comes back. This was seen in the
browser pane before the guard existed.

The addon is the official one rather than a local copy of its arithmetic,
because that arithmetic reads `xterm.js` internals (`_core._renderService`),
and the addon is released in step with the emulator that owns them.

### 5. Copy and paste, and a menu for them

Keys, on a guest whose shortcuts use Ctrl (`terminalClipboardKey`):

- `Ctrl+C` copies when text is selected, as in Windows Terminal, and is the
  interrupt otherwise.
- `Ctrl+V` pastes. The `^V` it used to send is a shell's quoted-insert, which
  nobody reaches for in a remote window.
- `Ctrl+Shift+C` and `Ctrl+Shift+V` always copy and paste, as in a Linux
  terminal.

The keys are physical (`KeyC`, `KeyV`), so they work under a Russian layout.
On a Mac guest none of this applies: `⌘C` and `⌘V` never reach the shell, and
the webview copies and pastes them itself.

The keys are handed back to the webview rather than handled here. The
emulator already answers the webview's own `copy` and `paste` events, bracketed
paste included, and a paste the webview does itself needs no permission to
read the clipboard.

A right click on the shell opens the terminal's own menu: Copy (disabled when
nothing is selected), Paste and Select all, with the shortcuts beside them.
The menu has no native event to ride on, so it uses `navigator.clipboard`. It
closes on a pointer press outside it, on Escape, and after any item. Its
position is set through the CSSOM rather than a `style` attribute, so no
content security policy can refuse it.

## Found, not changed: the emulator's styles are refused in a release build

The screenshot's missing cursor, proportional font and missing colours are
not a terminal bug. `tauri.conf.json` says `style-src 'self' 'unsafe-inline'`,
which ADR 0081 added so that `xterm.js` could append its own `<style>`
elements. But `dangerousDisableAssetCspModification` is `false`, so at runtime
Tauri adds a nonce and hashes to `style-src`
(`tauri-2.11.5/src/manager/mod.rs`, `replace_csp_nonce`). A browser ignores
`'unsafe-inline'` in any directive that carries a nonce or a hash. Every
`<style>` that `xterm.js` creates is then refused: the cell metrics, the theme
and the cursor.

ADR 0081 tested its directive on a page without Tauri's nonce, which is why it
looked right. The `vite` dev server applies no policy at all, so the terminal
looks right there too.

Reproduced on the built `view.html` with the mock IPC, in the browser pane:

- Under `style-src 'self' 'unsafe-inline' 'nonce-…'`, the page's own
  `<style>` carries the nonce, as Tauri gives it. The terminal is a
  proportional font with no colours and no cursor, which matches the
  screenshot.
- Under `style-src 'self' 'unsafe-inline'`, the same build renders correctly.

The fix is one line: `"dangerousDisableAssetCspModification": ["style-src"]`.
Tauri then leaves `style-src` as written, so ADR 0081's `'unsafe-inline'`
applies. `script-src` keeps its nonces. This ADR does **not** make that change:
it relaxes the policy of every window, so it is left to the owner. Until it is
made, sections 2 and 3 are invisible in a release build, and the cell grid is
unmeasured.

## Consequences

- A host has to run a build with section 1 before its shell prints colour. An
  older Mac or Linux host still starts its shell with no `TERM`, and the guest
  cannot correct that.
- `the_shell_knows_its_terminal_draws_colour` proves section 1 only when the
  tests run without those values of their own
  (`TERM=dumb COLORTERM= cargo test`), which is what a Lumepeer started from
  the Dock has.
- A host with no `xterm-256color` terminfo entry, such as a stripped-down
  container, would have `less` and `vim` refuse the terminal. No supported
  host is one.
- A paste from the menu can be refused by a webview that asks before a page
  reads the clipboard. The keyboard paste does not have that problem, because
  the webview performs it.
- `@xterm/addon-fit` is a new frontend dependency. It has to move with
  `@xterm/xterm`.
