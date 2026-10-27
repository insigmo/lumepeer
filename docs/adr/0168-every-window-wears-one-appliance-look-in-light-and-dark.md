# ADR 0168 — Every window wears one appliance look, in light and in dark

Status: accepted
Date: 2026-10-27

Amends decision 1 of
[ADR 0117](0117-the-window-follows-the-system-and-asks-about-updates-once.md)
("only the main window changes").

## Report

The owner asked for every window to be "very beautiful, simple and
minimalistic", with a light and a dark theme. What they found wrong with the
old look: it read as a stock dark admin panel, the six windows looked like
different programs (the host bar had its own slate palette, the view window
and the terminal their own near-blacks and a light blue), and small flaws in
spacing and contrast.

## Decision

### 1. One visual world: a household appliance

The direction was chosen from a decision round (PRODUCT.md, DESIGN.md): a
well-made appliance on the desk. A satin body (`#f4f4f2` light, anthracite
`#1b1b1c` dark), faceplates one step lighter with a moulded lower edge, small
lowercase legends, and colour only where it signals:

- **yellow** `#f2a31b` is the one action — the Connect key, a dialog's "yes",
  Upload/Download — always with charcoal text, in both themes;
- **green** is live, **red** is stop, **amber** is a warning;
- a **readout** — a charcoal window, dark in both themes — holds a live value:
  the invite code, a card's last picture of its machine, the video stats.

Every button is a round-ended key (`--radius-key`). A disabled yellow key is
grey, never a paler yellow, so yellow only ever means "press this". Focus and
the keyboard cursor are drawn in ink (`--focus`), because the key's yellow is
too light against the body to be seen as an outline. An offline host is an
unlit lamp (a hollow ring), not a red alarm.

All of it lives in `tokens.css`. A colour written anywhere else is a colour
that is wrong in one of the two themes.

### 2. Every window follows the theme

ADR 0117 left the view window and the two bars dark on purpose. They now
follow System / Light / Dark like the main window: the view window's
background, toolbar, banners, chat and modals; the host bar; the file manager;
the terminal, whose emulator now has a light palette (`TERMINAL_THEME_LIGHT`)
beside the dark one. The remote picture itself is never recoloured.

One exception stays: the agent bar ("remote access is active on this
computer", ADR 0085) is red in both themes. It is a signal, not chrome, and a
signal that turns pale in a light theme is one somebody stops seeing.

### 3. A choice made in one window reaches the others

Every window of the app shares one origin and so one `localStorage`.
`followTheme()` applies the stored choice and re-applies it on a `storage`
event and on a system light/dark switch; `onThemeChange()` is for what CSS
cannot reach — a running terminal swaps its palette in place.

### 4. The terminal's chrome is one stylesheet

The header, state lamp, Close key and right-click menu were styled only inside
`view.html`; the terminal window of ADR 0131 drew its Close as a bare browser
button and its menu unstyled. Both pages now link `terminal-chrome.css`.

## Consequences

- `view.html`, `hostbar.html`, `agentbar.html` and `terminal.html` link
  `tokens.css`; their inline styles carry no literal colours beyond white
  halos on the red signals, and the amber unread dot stays in the inline SVG
  icons.
- Not seen on a real machine yet: verified in the Vite dev server with a faked
  IPC (screenshots in both themes of every window), and by the unit tests.
  The webviews must support `color-mix()` (the view toolbar's plate), `:has()`
  (the live card's green ring) and the `lh` unit (the card's two-line status
  area): WebView2 is current; WKWebView needs Safari 16.4+, WebKitGTK 2.42+.
  An older engine drops those three details and nothing else.
