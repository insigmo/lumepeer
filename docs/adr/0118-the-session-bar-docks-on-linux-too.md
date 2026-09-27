# ADR 0118 — The session bar docks on Linux too: pinned by min = max, and drawn through XWayland

Status: accepted
Date: 2026-09-27

Extends [ADR 0055](0055-an-always-on-top-session-bar-for-the-host.md).

## Context

ADR 0055's bar collapses to a 20×58 tab at the screen edge. On Linux it did
neither half of that.

**The tab was the size of the card.** The bar is built with
`resizable(false)`, and GTK 3 sizes a window the user cannot resize from its
content: `gtk_window_update_fixed_size` pins min = max to the larger of the
requested size and the content's natural size. A `WebKitWebView`'s natural
size is the page it is already showing (or 200×200 before it knows), so
`host_bar_expand`'s `set_size(20, 58)` was clamped straight back up. The page
still drew its collapsed state — one chevron button filling the window — so
the host got a large square with a `<` in the middle. On X11 the bar still
moved, anchored by the right edge for a 20-pixel window, which is the
"it slides off sideways" half of the report. A probe with the same GTK and
WebKitGTK calls tao and wry make measured it: 200×200 after the collapse on
both X11 and Wayland, and 20×58 once the window was resizable and pinned by
geometry hints instead. This is the known tao/Tauri limitation (tauri#5876,
tauri#5679, tao#545).

**On Wayland the bar was an ordinary window.** Nothing in `xdg-shell` lets a
client place its own window or keep it above others, so `position` and
`always_on_top` were ignored: the compositor put the bar in the middle of the
screen, like any other window, and let other windows cover it. That breaks the
§2.2 guarantee ADR 0055 exists for.

The comparable tools get their panel to the edge by being X11 clients:
TeamViewer's Qt runs on `xcb`, AnyDesk is an X11 GTK application, and both run
under XWayland on a Wayland desktop. RustDesk draws natively on Wayland, and
its connection manager there is a plain 300×490 window that docks nowhere.
`wlr-layer-shell` would dock a native surface, but GNOME does not implement
it, and GNOME is the default Wayland desktop of the distributions this app
ships for.

## Decisions

### 1. On Linux the bar is resizable in name and pinned by min = max

`open_host_bar` builds the bar `resizable(true)` on Linux, with a minimum and
a maximum inner size both equal to the open card. `host_bar_expand` moves that
pin to the new shape before it calls `set_size`. GTK then has no content size
to fall back to, and the window manager has no range to let a person drag the
window through. Windows and macOS keep `resizable(false)`: their resize
already worked, and nothing about them changes.

### 2. On Linux the collapse moves the bar only after the resize has landed, and measures it without the frame

Once the bar could shrink, a live run on GNOME (Mutter on Xorg) showed two
more ways the tab missed the screen edge:

- An X11 window manager applies a resize when it gets to it. A move that
  arrives first is kept on screen by the size the window still has, so Mutter
  put the 20-pixel tab where the 262-pixel card's left edge had been, 242 px
  in from the edge, and each expand walked the bar further in.
  `host_bar_expand` now waits for the window to report its new size (at most
  half a second) before it calls `set_position`. Measured on that machine,
  the wait took 48–89 ms.
- The outer frame Mutter reports for an undecorated window includes an
  invisible 37 px band on top, while `set_position` places the window itself.
  Anchoring on `outer_position` and `outer_size` moved the bar up by half of
  that band on every collapse and every expand. On Linux the anchors are now
  read from `inner_position` and `inner_size`, the same frame `set_position`
  writes. Windows and macOS keep the outer frame, which is what their
  `set_position` writes.

Measured after the change on that machine (1718×920): open 262×188 at
(1456, 366); collapsed 20×58 at (1698, 431), flush with the right edge and
centred on the card; expanded again at exactly (1456, 366).

### 3. The process prefers X11, so a Wayland session draws it through XWayland

Before Tauri starts GTK, `main` calls `gdk::set_allowed_backends("x11,wayland")`.
On an X11 session nothing changes. On a Wayland session the windows become
X11 windows under XWayland, where placing a window and keeping it above
others work on GNOME and KDE alike, so the bar docks at the right edge and
stays on top as it does on X11.

`wayland` stays in the list, second, so a compositor running without XWayland
still gets a working app, only with the bar where the compositor puts it. A
`GDK_BACKEND` the user sets still overrides the list, as GDK always allows.

Screen capture and input injection do not follow this choice. They pick the
portal from `XDG_SESSION_TYPE`, which still says `wayland`.

`gdk::set_allowed_backends` rather than setting `GDK_BACKEND`: it is a safe
function, where `std::env::set_var` is `unsafe` in edition 2024 and this crate
forbids `unsafe`. It also leaves the environment of the sidecars this process
starts untouched. The `gdk` crate is the exact version `tao` already links.

## Consequences

- The collapsed bar is a 20×58 tab on Linux, and on a Wayland desktop it sits
  at the right screen edge and stays above other windows, as on X11, Windows
  and macOS.
- Every window of the app, not only the bar, is an XWayland window on a
  Wayland desktop. Under fractional scaling (125 %, 150 %) GTK 3 on X11 can
  scale only by whole numbers, so the UI may look softer than a native Wayland
  window. That is the cost of the option chosen over a layer-shell bar, which
  would have left GNOME's bar in the middle of the screen.
- A user who prefers native Wayland can start the app with
  `GDK_BACKEND=wayland`. The app still works; only the bar does not dock.
- Measured live on GNOME on Xorg only. On a Wayland session the app was run
  under WSLg, where its windows came up on XWayland and the tab measured
  20×58, but WSLg's window manager does not place windows the way a desktop
  does. Docking on GNOME or KDE under Wayland has not been measured yet.
- The `pilot` build's capability (`capabilities-pilot/pilot.json`) now covers
  the `hostbar` window too, so an e2e run can press the bar's buttons and read
  its size. Release builds never read that directory.
