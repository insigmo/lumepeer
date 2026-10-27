# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Two audiences in equal measure, confirmed by the owner (2026-10-27):

- People who help family and friends or reach their own computers. Many are not technical; the first connection must be obvious without reading anything.
- IT support and administrators who keep the window open for hours, manage many saved machines, and want density and speed over hand-holding.

## Product Purpose

Lumepeer connects to another computer and lets you use it as if you were there: view and control the screen in real time, move files and clipboard, talk over audio and chat, work across monitors, and reach trusted devices with nobody at the far end. Success is a session that starts in seconds and never makes either side wonder what the other can see or do.

## Positioning

Peer-to-peer and account-free: devices find each other by invite code and connect directly, falling back to relays, with no vendor server owning the session. The person at the host always controls what the guest can access and can change or revoke it mid-session. One app on Windows, Linux and macOS.

## Operating Context

Six windows, each a Tauri webview with its own chrome (`decorations: false`, custom title bar; traffic lights on macOS):

- **Main window** (`index.html`): own invite code, connect by code, saved hosts with presence, settings (theme, language, unattended access, updates, licensing).
- **View window** (`view.html`): the remote screen with a floating toolbar (input, clipboard, chat, files, terminal, recording, quality, fullscreen) and stats overlay.
- **Host bar** (`hostbar.html`): small always-on-top card over the host's desktop listing connected guests, docked to a screen edge, collapsible to a tab.
- **Agent bar** (`agentbar.html`): a strip telling whoever sits at the host that remote access is active.
- **File manager** (`files.html`): two panes, this computer and the remote one, plus a transfer queue.
- **Terminal** (`terminal.html`): a remote shell (xterm.js) filling its window.

The app is used day and night, often beside other tools for hours.

## Capabilities and Constraints

- Frontend: lit-html templates and plain CSS, built with Vite; no framework, no CSS library.
- Installed builds must need no runtime dependencies: no hosted fonts or CDN assets; CSP allows `style-src 'self' 'unsafe-inline'`, `img-src 'self' data:`.
- Theme: System / Light / Dark, stored per window in `localStorage` (`theme.ts`, ADR 0117). As of 2026-10-27 every window follows the theme, including the view window's chrome and both bars; the remote picture itself is never recoloured.
- 13 locales (ar, de, en, es, fr, it, ja, pl, pt, ru, tr, uk, zh), including right-to-left Arabic; layouts must survive long German and Russian strings.
- Accessibility and keyboard navigation are covered by tests (axe-core, keyboard-nav tests).

## Brand Commitments

- Name: Lumepeer.
- Logomark (`apps/desktop/src/logo.ts`): rounded-square tile, dark navy fill `#0F1724`, abstract blue-gradient "L" with a small violet triangle at its foot and a solid blue dot at bottom-right. The mark stays; it is the one fixed brand asset.

## Evidence on Hand

No testimonials, customer names, benchmarks or press exist. Nothing of the kind may be invented in the interface.

## Product Principles

1. The host is in charge: what the guest can do is always visible on the host's side and revocable in one action.
2. First connection without instructions: a non-technical person reaches a working session from the main window alone.
3. Stay out of the way: the windows frame someone else's screen or sit over someone else's work, so chrome stays quiet and small.
4. One product, six windows: every window speaks the same visual language.

## Accessibility & Inclusion

Keyboard operable throughout, screen-reader labelled, WCAG AA contrast in both themes, right-to-left layout for Arabic.
