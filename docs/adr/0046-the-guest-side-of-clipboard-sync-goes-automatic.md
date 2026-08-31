# ADR 0046 — The guest side of clipboard sync goes automatic, behind a mandatory indicator

Status: accepted
Date: 2026-08-31
Extends: ADR 0029 (the four independent grants become issuable), ADR 0030
(the clipboard reaches the OS, and each direction is one grant), ADR 0027
(the dial leaves the actor loop, and so does the frame poll)

## Context

Host-to-guest clipboard sync has been automatic since ADR 0030: the host's
own `clipboard_read` grant turns a watcher on, and a copy the host user makes
travels to the guest with no press on either side. Guest-to-host stayed
manual — `toolbar.ts`'s clipboard button read the guest's own clipboard with
`navigator.clipboard.readText()` on a press and called `clipboard_push`.

ADR 0030 explained why deliberately: "That gesture is the reason the guest
has no clipboard watcher. A guest that polled its own clipboard and shipped
every change to a host that may not even be allowed to receive it would be
leaking from the *guest's* machine, to decide something the guest cannot see
the answer to." A guest holds none of the four independent grants (ADR
0029) — `Grants::from_role` never sets any of them on the guest's own
`ViewState.grants` — so the press was standing in for a decision the guest
otherwise has no way to make: whether to let its own clipboard leave the
machine at all.

The user complaint (`docs/bugs/10-clipboard-auto.md`) was that this reads as
a broken feature rather than a considered one: "убери кнопку буфера обмена
… сделай так чтобы буфер обмена был автоматическим". `DECISIONS.md`'s D5
settles it as a product question — automatic, both ways — with one condition
attached that this ADR exists to keep honest: a visible sync indicator on
both sides is not optional, because §2.2's "no hidden capture" is a rule
about capture of *this machine's* own data, not only about the screen it
shows someone else.

## Decision

### The gesture is replaced by an indicator, not removed outright

The guest's clipboard still leaves the guest's machine only because a
person can see that it does. What changes is *when* that visibility has to
be checked: previously, at the moment of a press; now, continuously, as a
permanent status the guest never has to seek out. The toolbar's clipboard
icon stops being a button (`toolbar.ts::actions.sendClipboard` and
`ToolbarCommands.clipboardPush` are gone) and becomes a status — `title`
and `aria-label` say sync is automatic, and it is drawn as a `<span>`, not a
`<button>`, so nothing about it invites or answers a click. Next to it, a
transient note (`toolbar-clipboard-note`, aged out after `CLIPBOARD_NOTE_MS`
— the same constant and the same shape as the host panel's own
`clipboardSynced` note in `session-status.ts`) appears whenever the host's
clipboard arrives, polled through the newly-added
`ToolbarCommands.clipboardPull`.

Nothing about the *authorization* changes. The host's `clipboard_write`
grant is exactly as load-bearing as it was: `network.rs::on_inbound`'s
`MessageKind::ClipboardSync` arm still checks it before writing to the
host's real clipboard, unchanged by this ADR. What the guest's press used to
stand in for — "I chose to offer this" — is replaced by "the indicator is
what I chose to accept", the same trade §8.2 already makes for the host's
own `clipboard_read` switch: turning it on is the host's one-time decision
that every subsequent copy may cross, not a decision repeated per copy.

### Why automatic is acceptable here specifically

Three things hold together, and D5 requires all three:

1. **The grants are unchanged.** `IndependentGrant::ClipboardRead` and
   `ClipboardWrite` are exactly as before (ADR 0029). "Automatically" means
   "without a press", not "without a permission" — the host still decides,
   per session, whether either direction is live at all, and switching one
   off stops it immediately (`refresh_clipboard_watch`, `on_set_grant`).
2. **Content never renders, logs or persists**, on either side, before or
   after this change. §15 is unchanged; see Verification.
3. **The indicator is mandatory and cannot be hidden or disabled** — the
   same property §17's recording banner and D1's unattended-access line
   already have. A feature that reads a person's own clipboard without
   asking and without saying so is the hidden capture the ground rules name
   explicitly; a feature that reads it and *says so, continuously*, is a
   background sync with an honest status light. This ADR treats the
   difference between those two as the entire question, and answers it by
   making the indicator part of the decision, not a UI nicety layered on
   afterward.

### The guest's clipboard read moves into Rust, on the existing thread

`network.rs`'s actor already owns a `ClipboardWorker` per node
(`clipboard_os.rs`) for the host role. This ADR extends its use to the
guest role of the same node: `refresh_clipboard_watch` now also turns
watching on whenever `self.views` is non-empty — this node has an open view
onto at least one host — and `on_local_clipboard` now also offers the
change to every peer in `self.views`, through the same
`on_clipboard_push` a manual press used to call. `start_view` and
`stop_view` call `refresh_clipboard_watch` so the watcher tracks a view
opening and closing exactly as it already tracked a grant changing.

Doing this in the webview instead — polling `navigator.clipboard.readText()`
on a timer — was rejected for the reason ADR 0027 exists at all, applied to
a second blocking operation: a clipboard read is a round trip to whichever
application currently owns the selection, and on X11 that application can be
slow or wedged. A poll in the webview would either run on the UI thread (and
occasionally stall the picture) or need its own scheduling machinery
duplicating what `clipboard_os.rs` already provides on a dedicated OS
thread that the actor never blocks on. There is also no way to poll
`navigator.clipboard` at all without either a user gesture or the
`clipboard-read` *browser* permission — which the view window's capability
manifest (`capabilities/view.json`) has never granted and does not grant
here, and which would in any case be a standing browser permission, live
whether or not a session exists, exactly the shape `clipboard_os.rs`'s own
header rejects for the Tauri clipboard plugin. Reusing the Rust-side worker
sidesteps both problems: the read happens off the actor loop on the same
thread and cadence (`CLIPBOARD_POLL_INTERVAL_MS`, unchanged) the host side
already relies on, and the webview keeps no clipboard capability at all
beyond pulling an already-arrived, already-applied payload.

### The webview gains a capability, and loses one

`capabilities/view.json` drops `allow-clipboard-push`: with the push path
gone from the webview entirely, granting a view window the ability to call
it would be exactly the "permission nobody uses is an open door" the source
document already argues against elsewhere in this codebase. It gains
`allow-clipboard-pull`, needed for the mandatory indicator's poll. This is
not a widened grant in the §8.2 sense — `clipboard_pull` only ever returns
this session's own already-authorized, already-applied inbound payload
(the text is on this machine's real clipboard already, by the time
anything can pull it), the same command the main window has called since
ADR 0030, now reachable from the view window it was always written to
support (`commands.rs::clipboard_pull` already checks
`check_view_window(&window, &peer).or_else(|_| check_window(&window))` —
this ADR is the first thing to actually exercise that view-window branch).

## Consequences

- Clipboard sync is symmetric for the first time: both directions are
  automatic, both sides carry a mandatory indicator, and neither side's
  indicator can be turned off or hidden.
- `toolbar.ts::ToolbarCommands` no longer has `clipboardPush`; a webview
  cannot offer a clipboard payload to a peer through any surface reachable
  from a view window. `grep -rn clipboard_push apps/desktop/src` is empty.
- The guest's own clipboard is now read continuously while a view is open,
  not only on a press. This is the change D5 asked for and the reason this
  ADR exists — recorded here rather than left implicit, per ADR 0001.
- A revoke of `clipboard_write` on the host stops the guest's offer from
  landing, exactly as before; it does not and cannot stop the guest's local
  watcher, because the guest holds no grant to revoke (ADR 0029) — the
  watcher is gated on the guest's own open view, per `refresh_clipboard_watch`.
- `docs/bugs/14-clipboard-files.md` inherits this shape: a future file-list
  read on the guest side should gate the same way, on an open view rather
  than on a grant the guest does not hold.

## Verification

- `network.rs`: `a_guest_clipboard_change_reaches_the_host_only_with_the_write_grant`
  — a local change on the guest's machine reaches the host's real clipboard
  only once `clipboard_write` is granted, over two real actors and a real
  control connection, the same substitute for cross-machine testing
  `clipboard_pair`'s existing tests already use.
  `closing_the_view_stops_the_guests_own_clipboard_watch` — leaving the
  session stops the guest's own watcher, mirroring
  `a_revoke_stops_the_clipboard_watcher` on the host side.
- `clipboard_os.rs`'s existing suite is unchanged and still holds: it is
  exercised identically regardless of which role calls `set_watching`.
- `toolbar.test.ts` and `clipboard-ui.test.ts`: the clipboard element is not
  a `<button>`, carries a `title`/`aria-label` naming it a status, the
  transient note appears and ages out on `CLIPBOARD_NOTE_MS` exactly like
  the host panel's, and the pulled text never reaches
  `container.textContent`.
- `i18n.test.ts`: both locales still cover every key touched here.
