# ADR 0081 — The terminal emulator writes its own stylesheet, so `style-src` gains `unsafe-inline`

Status: accepted
Date: 2026-09-10

Follows [ADR 0079](0079-a-terminal-is-its-own-grant-and-never-the-clients-privileges.md),
whose decision 6 makes the guest's terminal `xterm.js` and rules out writing
one. This is the bill for that decision, and it is a change to §13's content
security policy, so it does not get made quietly.

## Context

§13 pins one policy for every window this app opens
(`apps/desktop/src-tauri/tauri.conf.json`, `app.security.csp`), and it is
deliberately tight: `default-src 'self'`, `script-src 'self'`,
`style-src 'self'`, `img-src 'self' data:`, `connect-src 'self' ipc:
http://ipc.localhost`, `frame-src 'none'`, `object-src 'none'`,
`base-uri 'none'`.

`xterm.js` 6.0.0 builds three stylesheets while it runs, each by creating a
`<style>` element and assigning its `textContent`:

- the scrollbar slider's colours, on the scrollable element;
- the cell metrics — the `display`, `height` and `vertical-align` that make a
  row a row — recomputed on every resize;
- the theme, injected whenever the colour set changes.

A `<style>` element is an inline style block whatever created it, so
`style-src 'self'` refuses all three. That is not a guess: a page served under
exactly this directive, doing exactly this (create the element, set
`textContent`, append), leaves the rule unapplied and logs

> Applying inline style violates the following Content Security Policy
> directive 'style-src 'self''.

Without those three the terminal is not a degraded terminal. It is unmeasured
text with no cell grid, no theme and a scrollbar that is not there — a
rendering failure that reads, to anybody looking at it, like the remote end
misbehaving.

## Decision

`style-src` becomes `'self' 'unsafe-inline'`. Every other directive stays
exactly as it was.

What that does **not** open is the point of writing it down. `script-src`
does not move: no inline script, no `eval`, and every script on every page is
built from this repository. `default-src`, `img-src` and `connect-src` do not
move either, so the exfiltration channel that makes `unsafe-inline` styles
interesting elsewhere — an attribute selector paired with a
`background-image: url(https://…)` — has nowhere to send anything: every
resource a stylesheet can name is same-origin, and every origin but this one
is refused. What is left is defacement by injected CSS, which needs a markup
or script injection to arrive through, and the directive that would have to
give way for that has not.

The cost is real and is that this is one policy for the whole application:
Tauri 2 has a single `app.security.csp`, so the main window and the host bar
get the looser `style-src` too, for a capability only the view window uses.

## Consequences

- The terminal renders, on every platform, without a second rendering path or
  a fork of the emulator.
- The emulator's own stylesheet is still a real file: `view.ts` imports
  `@xterm/xterm/css/xterm.css` statically, so the bundler emits it and the
  page links it. A dynamic `import()` of that CSS would arrive as another
  inline `<style>` and would have been the same problem in a place nobody
  would think to look for it.
- If `xterm.js` ever moves to constructable stylesheets — `new CSSStyleSheet`
  plus `adoptedStyleSheets`, which CSP does not govern — this decision can be
  reverted with no change to anything else.

## Alternatives considered

- **A nonce.** The clean answer, and unavailable: `xterm.js` 6.0.0 has no
  `nonce` option in its typings or its bundle, and the elements are created by
  the library rather than by anything here.
- **Patching `document.createElement` to stamp Tauri's own nonce.** A
  monkey-patch on a DOM primitive, whose correctness depends on scraping a
  nonce off some other element that happened to get one. A worse thing to have
  to reason about than the directive it would be protecting.
- **The canvas or WebGL renderer instead of the DOM one.** It replaces how
  rows are drawn, not the three style elements above — two of them are in the
  core viewport, not in the renderer at all.
- **Writing the emulator.** Ruled out by ADR 0079 decision 6 and by
  `docs/gap-tasks/17-remote-terminal.md`, which says it in as many words.
- **A second, looser policy for the view window only.** There is one
  `app.security.csp` in Tauri 2. Serving that window from a different origin
  to get it its own policy would be a larger change to §13 than this one.
