# ADR 0029 — The four independent grants become issuable, through the core only

Status: accepted
Date: 2026-08-26
Extends: §8.2 (roles and grants), §2.2 (the six permissions), §2.3 (only the
host's core authorizes)

## Context

§2.2 names six permissions and requires them to stay independent: `view`,
`input`, `clipboard_read`, `clipboard_write`, `file_transfer`, `recording`.
`Grants` (`crates/core/src/consent.rs`) has carried all six since phase 1, and
`Grants::from_role` has always been explicit that a role implies at most `view`
and `input` — the other four start `false` and no role turns them on.

What was missing is the other half of that design. `SessionManager` had `grant`,
`revoke`, `grants` and `active`, and no way at all to move one of the four. The
consequence was not a missing switch in the UI; it was that the four grants were
unreachable in principle:

- `on_record_toggle` (`apps/desktop/src-tauri/src/network.rs`) gates on
  `g.recording` — always `false`, so §17 recording could never start.
- The clipboard paths gate on `g.clipboard_read` and `v.grants.clipboard_write`
  — always `false`.
- `file_transfer` gates the lazy `rd/file/1` connection — always `false`, so the
  transfer engine had no way to be reached.

Three shipped features were dead behind a check that could not become true. The
deny-by-default rule was being enforced by the absence of a code path rather
than by a decision, which reads the same from the outside and is not the same
thing: there was nothing for the host to decide.

## Decision

**One mutator, and it can reach exactly four of the six grants.**

`IndependentGrant` (`crates/core/src/consent.rs`) enumerates
`ClipboardRead`, `ClipboardWrite`, `FileTransfer`, `Recording` — and nothing
else. `SessionManager::set_grant(peer, which, allowed)` is the only way to move
one. `view` and `input` are not expressible as an argument to it, so no caller
outside `crates/core` — the webview least of all — has a path to input that
does not go through the host choosing a controller role. That is the point of
the split, not an incidental narrowing: a "toggle a grant" call that could
carry `Input` would be a role change wearing a checkbox.

`Grants::get`/`Grants::set` match on all four variants with no `_` arm, so a
seventh permission cannot appear and silently read as denied.

**A grant belongs to a session, not to a peer.** `set_grant` requires
`SessionState::Active` and returns `CoreError::NotPermitted` otherwise —
including inside the reconnect window, where the picture is not on screen and
the host is not watching. `revoke` removes the session outright, so the four go
with it, and the next `grant` starts again from `Grants::from_role`, which
grants none of them. A guest that was allowed files once has to be allowed them
again.

**Grants do not travel on the wire.** No new `MessageKind`, no
`PROTOCOL_MINOR` bump, no `FEATURE_*` bit. Each side checks its own copy
already, and the guest learns a grant moved by finding its next clipboard or
file attempt permitted or refused. Adding a message would have created a second
statement of the same fact that could disagree with the first.

**Switching a grant off also stops what it was already paying for.** "Refused
at the next attempt" is the whole rule for a grant nobody is currently
spending, and it is not enough for the three that have something running
behind them. The host actor therefore treats an off-switch as a small revoke
of exactly one permission: `clipboard_read` stops the clipboard poll on the
spot (ADR 0030), `file_transfer` closes `rd/file/1` and cancels every transfer
and staging file of that peer without exporting any of it, and `recording`
stops the recorder and tells the guest with the same `RecordAck(false)` a
manual stop sends. `clipboard_write` is the one with nothing to stop: it is
spent by the peer, and every arriving payload is checked against the live
grant.

Anything else would have made the switch a statement about the future only. A
host user who moves `recording` off has to be able to read "nothing is being
recorded" off the same panel, and §4's rule that a revoke must not queue behind
a 500 MiB transfer applies no differently to the finer switch beside it. The
teardown is deliberately *not* `drop_file_state`, which also forgets the peer's
`FEATURE_FILE_TRANSFER` advertisement: that is a fact about a connection which
is still up here, and forgetting it would make switching the grant back on a
silent no-op. Withdrawal costs the grant and nothing else — turn it on again
and the next file moves.

**The audit event is returned, not logged.** `set_grant` hands back
`AuditEvent::GrantChanged { grant, enabled }`; `SessionManager` holds no sink,
and inventing the wiring here would have pre-empted the §15 storage work. The
desktop actor logs it through `tracing` against the pseudonymized label. Which
permission moved and in which direction is recorded; clipboard content, file
names and the raw `NodeId` are not.

**Main window only.** `session_set_grant` calls `check_window`, not
`check_view_window`, and the permission is in `capabilities/main.json` alone,
never in `capabilities/view.json`. A view window is the *guest's* side of a
session; a guest reaching this command would invert the whole model.

## Consequences

- Clipboard, file transfer and recording become reachable — but only after a
  host user says so, per session, per permission. This widens what the app can
  do without widening what a role means, which is what §2.2 asks for.
- `SessionStatusDto` now carries the four grants, so the status list can show
  what a session actually holds instead of guessing. It picks up
  `struct_excessive_bools` for the same reason `Grants` does, with the same
  justification: folding them into anything denser hides the independence the
  spec requires.
- The switches show what the last `session_status` reported and never toggle
  optimistically. A refused change re-polls, so a switch cannot sit in the
  "on" position against a core that said no.
- A running session's grants can now change mid-session, which the snapshot
  rule did not previously have to consider. It still holds where it was aimed:
  the change comes from the host user acting on *this* session, not from an
  edit to `config/control_policy.toml`, and the allowlist snapshot taken at
  grant time is untouched by this path.
- `AuditEvent` gains a variant at the end of the enum. It is not serialized on
  the wire, so no golden vector moves.

## Verification

- `crates/core`: every one of the four turns on and off and leaves the other
  three alone; `set_grant` is refused for an unknown peer, a pending request,
  a reconnecting session and an ended one; `revoke` drops the four and a fresh
  `grant` does not restore them.
- `set_grant_never_moves_view_or_input` — a proptest over arbitrary sequences
  of `set_grant` against all three roles, asserting `view` and `input` stay
  exactly where `Grants::from_role` put them.
- `an_independent_grant_dies_with_the_session_that_held_it`
  (`tests/integration/tests/consent_cycle.rs`) — the same property across a
  real control connection and a real revoke.
- `session-grants.test.ts` — four switches and only four, off on a fresh
  session, on only because the core said so, the IPC call carrying the right
  grant and direction, a re-poll after both a success and a refusal, no
  switches on a pending row, and every switch keyboard-reachable and named in
  both locales.
- `withdrawing_the_grant_ends_the_transfers_and_can_be_given_back`
  (`network.rs`) — a live transfer, the switch off, the host's transfers and
  offers gone and a new offer refused by the core, then the switch back on and
  a second file moving end to end.
- `withdrawing_the_grant_stops_a_running_recording` (`network.rs`) — the
  recording stops, `recording_active` goes false, the guest's frame flag goes
  dark, and the file is closed off properly with its stop event rather than
  abandoned.
