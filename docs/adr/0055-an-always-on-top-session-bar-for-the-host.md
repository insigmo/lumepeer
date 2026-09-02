# ADR 0055 — An always-on-top session bar for the host

Status: accepted
Date: 2026-09-02
Extends: design doc §2.2 (no hidden capture, and the host can always stop it),
§13 (per-window IPC capabilities)

## Context

The host's controls live entirely in the main window. Closing it hides the app
to the tray (`main.rs`, deliberately, so a session survives a stray click on
the X), and minimizing it leaves the tray icon as the only surface at all.
Both are ordinary things to do while someone is connected — the host is
usually working in a different application, which is why they invited a guest
in the first place.

That leaves the person at the machine with no visible answer to "who is
connected right now" and no reachable answer to "stop this", short of finding
the tray icon and restoring a window. §2.2 is a claim about what the operator
can see and do without going looking for it, and a taskbar button does not
satisfy it. Every comparable tool solves this the same way, with a small
always-on-top panel that docks to a screen edge and collapses to a tab.

## Decision

**A second host-side window, `hostbar`, up exactly while a guest is
connected.** Undecorated, always on top, out of the taskbar, unfocused on
open, docked to the right edge of the primary screen and draggable from its
own header.

**The actor owns its lifecycle; no window asks for it.** `ViewWindows` gains
`set_host_bar(visible)`, and the actor calls it from one place —
`reconcile_host_bar`, at the end of every turn of its `select!` loop —
whenever the count of sessions in `SessionState::Active` crosses zero. One
reconciliation rather than a call at each of the half-dozen paths that start
or end a session (a consent, a revoke, a disconnect, a session that timed
out), because a path that forgot to make the call would leave a bar up for a
guest who had left. "Somebody is connected to this machine" is not something
the untrusted presentation layer decides (§2.3), so there is no IPC command
that opens the bar.

A session inside its reconnect window is not `Active` and does not hold the
bar up: a bar that stayed after the guest's link dropped would be claiming
someone is watching when nobody is.

**Its capability is four permissions wide.** `capabilities/hostbar.json` names
`session_status`, `session_revoke`, and the bar's own two — `host_bar_expand`
(geometry, because an undecorated window has no chrome to resize itself by)
and `host_bar_focus_main` (raising the main window) — plus
`core:window:allow-start-dragging` scoped to its own label. Deliberately not
`core:default`, following `capabilities/view.json`. `session_status` and
`session_revoke` are widened in Rust by `check_host_surface`, which accepts
the main window and the bar and nothing else — the JSON is not the only place
the rule lives (§13).

**It is deliberately not a second UI.** Who is connected, at what role, a
revoke each, and one button that raises the main window. Settings, the audit
log, chat, files, recordings and the invite all stay where they were; a full
UI floating over every other application would be the opposite of staying out
of the way.

## Consequences

- Minimizing or closing the main window mid-session no longer takes the host's
  controls with it, which is the §2.2 gap this closes.
- The bar is on the host's own screen and is therefore captured into the
  stream the guest sees, like any other window. That is the same visibility
  the recording indicator already accepts by design.
- A third Vite entry (`hostbar.html`) and a third window label. The IPC
  surface grows by two commands, both host-side and both refused from every
  window but the bar.
- The collapse anchors the bar's *right* edge and clamps to the monitor it is
  on, so a bar the host dragged somewhere stays there and one docked at the
  edge does not walk inwards each time it is opened.
- Nothing here is configurable. There is no "hide the session bar" setting,
  for the reason the recording indicator has no off switch: an indicator the
  host can put away is not an indicator. Collapsing it to the tab is as small
  as it gets, and the tab is still on screen.
