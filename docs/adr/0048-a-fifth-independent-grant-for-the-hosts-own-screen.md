# ADR 0048 — A fifth independent grant for the host's own screen

Status: accepted
Date: 2026-08-31
Extends: ADR 0029 (the four independent grants), ADR 0010 (Wayland capture and
input), D7 point 2 (`docs/bugs/DECISIONS.md`)

## Context

`docs/bugs/16-host-display-mode.md` asks for the host to physically switch its
own monitor's resolution and refresh rate at a guest's request — not the
existing `StreamScaleRequest` (D7 point 1, `docs/bugs/13-stream-resolution.md`),
which only caps what the guest *receives* and never touches the host's actual
display. Changing the physical mode moves every window on the host operator's
desktop and, on some driver/monitor combinations, can leave the screen
unreadable until someone intervenes by hand. That is a materially different
risk from anything the five existing grants (`view`, `input`, `clipboard_read`,
`clipboard_write`, `file_transfer`, `recording`) cover, and the ground rules
(root `README.md`) are explicit that `view` implies nothing beyond itself.

ADR 0029 fixed `IndependentGrant` at four variants — `ClipboardRead`,
`ClipboardWrite`, `FileTransfer`, `Recording` — deliberately excluding `view`
and `input` so that no caller outside `crates/core` could ever express "give
this session input" as a plain toggle. That reasoning does not argue against a
*fifth* variant; it argues against ever collapsing `view`/`input` into the same
enum. A capability with its own blast radius, gated independently of every
role including `FullControl`, is exactly what `IndependentGrant` exists to
add.

## Decision

**`IndependentGrant` gains a fifth variant: `DisplayMode`.** `Grants` gains a
matching `display_mode: bool` field, defaulting to `false` in
`Grants::from_role` for every role — `FullControl` implies keyboard and mouse
and nothing else, exactly as it already implies neither `recording` nor
`file_transfer`. `Grants::get`/`Grants::set` stay exhaustive with no `_` arm,
so a sixth permission cannot appear later and silently read as denied.

**`view` is insufficient by construction, not by a runtime check someone could
forget.** A session holding `view` but not `display_mode` can watch the host's
screen and ask for a smaller picture (`StreamScaleRequest`) but the host's own
`on_display_set_mode` handler (`apps/desktop/src-tauri/src/network.rs`)
re-checks `grants.display_mode` from `SessionManager` fresh on every request,
the same re-check `on_monitor_select` already does for `view` — a grant
revoked a moment ago must not be honored by a message already in flight (§2.3).

**The grant gates two new messages, not a wire concept of its own.** ADR
0029's "grants do not travel on the wire" still holds: `IndependentGrant`
itself is never serialized. What is new, unlike the four grants ADR 0029
covers, is that this grant gates *host-to-guest* data as well as a
guest-to-host request — the mode *list* is exactly the kind of detail §18
says a host must not offer for something it would refuse to act on:

- `MessageKind::DisplayModesList { modes, reason }` (host to guest): the
  monitor's current modes, or an empty list with a
  `DisplayModeUnavailableReason` explaining why — `NotGranted`,
  `PlatformUnsupported` (Wayland, ADR 0010; macOS, out of scope this
  iteration) or `NoModesReported` (the platform enumerates but found
  nothing). Sent once per grant transition (session start, and again on
  `session_set_grant`), the same trigger `announce_monitors` uses for
  `MonitorsList`. `check_limits` enforces `modes.is_empty() ==
  reason.is_some()`: the two fields must never disagree about which state
  the list is in.
- `MessageKind::DisplaySetMode { mode_id }` (guest to host): a request
  against one of the ids the host itself most recently announced.

Both ride `PROTOCOL_MINOR 9` behind `FEATURE_DISPLAY_MODE`, following the
`FEATURE_STREAM_SCALE`/`FEATURE_CLIPBOARD_FILES` shape: a peer that never
advertised the string never receives or is trusted to have understood either
message.

**Reversibility is part of the grant, not a UI nicety layered on top**
(`docs/bugs/16-host-display-mode.md` task 3). The host remembers the mode a
peer's session found the monitor in on that session's *first* successful
switch, and restores it — always, including an ungraceful disconnect — from
the same per-peer teardown `on_closed` already runs for media, file transfers
and clipboard state, and from `on_set_grant`'s existing exhaustive match when
`display_mode` is switched off mid-session (mirroring `FileTransfer` and
`Recording` in that same match).

Two further safeguards, both new constants in `crates/core/src/constants.rs`:

- **Windows only:** `ChangeDisplaySettingsExW` is called once with `CDS_TEST`
  before the real, non-persistent apply. A driver that would refuse the mode
  is caught before anything on screen moves. X11 has no equivalent dry-run
  flag, so this step is Windows-specific, as `docs/bugs/16-host-display-
  mode.md` task 3 asks.
- **Both platforms:** `DISPLAY_MODE_CONFIRM_TIMEOUT_SECS` bounds how long an
  applied mode is allowed to stand unconfirmed before the host reverts it on
  its own. The task's own wording ("if the host has not confirmed it still
  sees the picture") reads two ways — a human at the host clicking
  something, or the host's own capture pipeline proving the new mode still
  produces frames. This ADR picks the second, deliberately: no new
  confirmation surface exists in this iteration's UI scope
  (`docs/bugs/16-host-display-mode.md` task 4 only adds the guest-facing
  selects), and a safety net that only fires when someone clicks a button
  that was never built is not a safety net. Confirmation is therefore the
  capture backend successfully restarting and handing back a frame at the
  new mode; failing to do so before the timeout reverts automatically,
  unattended host included. A future iteration that adds a real
  host-side "keep this mode?" prompt can tighten this without changing the
  wire contract.

**Wayland stays an honest empty list.** `ScreenCapturer::display_modes`
defaults to returning nothing (Task 1, already committed), and
`DisplayModesList.reason` reports `PlatformUnsupported` rather than the guest
inferring silence as a bug — the same complaint D7 point 2 exists to close.

## Consequences

- A guest with `view` and even `FullControl` still cannot move the host's own
  screen without a fifth, explicit, revocable grant — consistent with every
  other capability this project has added since ADR 0029.
- `PROTOCOL_MINOR` moves to 9; `tests/interop/golden_vectors.txt` gains
  vectors for both new discriminants, appended after minor 8's, with an
  `invalid` one for a `DisplayModesList` whose `modes`/`reason` disagree.
- `SessionStatusDto`/`session-status.ts`'s `GRANT_ROWS` gain a fifth switch,
  the same shape the other four already use — required for the grant to be
  reachable at all, the same completeness ADR 0029 insisted on for the first
  four.
- A host that changes its own monitor mid-session and then crashes, loses
  power, or has its process killed outright is not covered by the in-process
  teardown this ADR relies on: Windows' non-persistent
  `ChangeDisplaySettingsExW` call means a reboot alone restores the
  registry-configured mode, which is the best this iteration can promise
  without a session-0 watchdog process — a materially different piece of
  work this ADR does not attempt.
- macOS is untouched: `ScreenCapturer::display_modes`'s default keeps
  reporting an empty list there, same as Wayland, and `DisplaySetMode` on a
  macOS host is refused the same way a set-mode request against zero
  announced modes is refused everywhere else.
