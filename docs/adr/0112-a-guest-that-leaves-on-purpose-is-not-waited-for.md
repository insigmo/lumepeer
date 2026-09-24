# ADR 0112 — A guest that leaves on purpose is not waited for

Status: accepted
Date: 2026-09-24

Narrows [ADR 0089](0089-a-dropped-session-resumes-inside-its-window.md),
whose rule reads "a session that drops while granted is **parked**". That rule
stays for a link that drops. It no longer applies to a guest that closed the
connection itself.

## Context

The e2e matrix on 2026-09-24: a Windows guest opened a terminal-only session
to a Mac host and pressed Close in the terminal window. The window closed.
The guest's actor left through `on_revoke` → `stop_view` →
`close_connection_normal`, which closes the control connection with
`CLOSE_NORMAL`: the user leaving on purpose.

On the host, `on_closed` called `SessionManager::on_disconnect` and then
`park_session` for every session that had been granted, whatever code the
connection was closed with. The session sat in `Reconnecting` for
`RECONNECT_WINDOW_SECS`, which is five minutes. `SessionManager::grant` counts
every session it holds in `active_guest_count()`, parked ones included. On a
plan with one guest place, that means the host refused every other guest with
`ConcurrentGuestLimit { limit: 1 }` for five minutes. The Mac's log shows "peer
disconnected" at 12:36:08 and "a dropped session was not resumed inside its
window; it has ended" at 12:41:08. A Linux guest's grant between those two
times was refused. The parked session also still held the controller role,
which blocks a second full-control guest on any plan.

The person at the host could not end it either. The session list leaves out
`Reconnecting` sessions, so there was no row to revoke. ADR 0089 accepted that
for a real drop: "what they see is the session gone and then back". Here
nothing was coming back.

The terminal only happened to be where this was seen. Every view window a
guest closes goes the same way.

## Decision

### The close code decides

In `on_closed` (`end_or_park_session`), a connection the far side closed with
`CLOSE_NORMAL` (`closed_by_peer_with(CLOSE_NORMAL)`) ends its session with
`SessionManager::revoke` instead of parking it. The consent-rate counter is
forgotten, as it is for any session that ran
(docs/bugs/03-connection-list.md, task 2), so this does not bring back the
rate-limit bug that test `h1_…` guards against. A peer that was only queued
has its request dropped, as before.

Everything else still parks: a lost link, an idle timeout, a reset, any other
close code, and this side's own close.

### Why no resume is lost

A guest closes with `CLOSE_NORMAL` in exactly two places, and both are the
person leaving:

- **`on_revoke`, view branch**: the window closed. `stop_view` runs before the
  close, so `on_closed` finds no view to park and arms no wait. The guest
  never claims this session again.
- **`on_connect_cancel`**: the connect form's Cancel, while the host is still
  asking (`AwaitingConsent`, `AwaitingCredentials`) or while a resume is in
  flight (`Resuming`). `stop_reconnect_wait` runs first. In the first two
  cases there is no granted session to end. In the third, the user has called
  off the resume, and a session the host already restored over that
  connection is one they have just said they do not want.

A resume starts only from a connection that ended without the guest closing
it, and the host never sees `CLOSE_NORMAL` from such a connection. The host
closes with `CLOSE_NORMAL` in one place of its own:
`release_connection_claimed_by`, when a claim names a connection it still
holds. `closed_by_peer_with` does not count a close made from this side
(`LocallyClosed`, not `ApplicationClosed`), so that session is still parked,
and `on_resume_claim` finds it right after.

### The close arrives before the stream ends

`close_connection_with` closes the QUIC connection while it still holds the
handle, so the writer task's send stream is dropped only after the close.
noq 1.2.0, the QUIC stack under both transports, records the error as soon as
`close` is called, and `SendStream::drop` does not finish a stream once that
error is set. The
host's reader therefore fails on the application close, never on a clean end
of stream that arrived first, and `close_reason()` is set by the time
`on_closed` reads it. If the close frame is lost on the way (the process was
killed, or the link went down at the same moment), the host sees a lost link
and parks the session. That is ADR 0089's behaviour, bounded by its window.

## Consequences

- Closing a view or terminal window frees the guest's place, and the
  controller role, as soon as the host reads the close. A second guest can be
  granted straight away.
- A guest that cancels a resume after the host honoured the claim ends the
  session at the host. Before this, the host parked it a second time.
- A guest too old to send `CLOSE_NORMAL` closes with `CLOSE_MALFORMED`, and
  the host parks the session as before. Nothing gets worse for such a guest.
- ADR 0089's resume is unchanged for everything it was written for. Its tests
  still pass, with no changes to what they assert.
- Test (`crates/runtime/src/network.rs`):
  `a_guest_that_closes_its_window_frees_its_place_at_once`: a guest is
  granted full control and closes its window, and a second guest's grant then
  succeeds on the one-place plan every test actor runs on. Before the fix,
  this failed with `ConcurrentGuestLimit { limit: 1 }`. Existing tests guard
  the cases this ADR says still park or still behave as before:
  `a_claim_replaces_a_connection_the_host_has_not_seen_drop` (the host's own
  `CLOSE_NORMAL` must still park, or the claim behind it would be refused),
  `connecting_again_instead_of_resuming_asks_for_consent` (Cancel while
  `Resuming`), and
  `h1_reconnecting_past_the_rate_limit_keeps_working_after_a_clean_session`
  (the consent-rate counter is still forgotten when a guest closes its window).

## Still open

- A window closed while it is **parked** (ADR 0105), with its session away,
  has no connection to say so on. `on_revoke`'s parked branch calls the wait
  off and closes nothing. So the host still holds that session until the
  window runs out, and the person at the host still cannot end it by hand.
  Ending it would mean either dialing the host only to say goodbye, or
  listing parked sessions in the host's session list so they can be revoked.
  Both are separate decisions.
- Tested only between actors on one machine. The e2e matrix pair where this
  was seen, a Windows guest and a Mac host with a terminal-only session,
  should be run again.
