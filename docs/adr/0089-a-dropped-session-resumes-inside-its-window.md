# ADR 0089 — A dropped session resumes inside its window

Status: accepted
Date: 2026-09-17

Implements §10's session resume, which three ADRs recorded as missing:
[ADR 0077](0077-a-directory-is-a-manifest-and-the-ceiling-is-free-space.md) (a transfer cannot
survive a control-connection drop because the session does not),
[ADR 0083](0083-one-dial-plan-per-connect-and-a-fallback-before-consent.md) §5
(the app never sends `ResumeHello` and never calls `on_reconnect`) and
[ADR 0084](0084-restarting-the-host-is-its-own-grant-and-a-warned-act.md) §5,
whose "an ordinary blip repairs itself with the session and its grants intact"
was not true until now.

## Context

`crates/core` and `crates/net` had the pieces: `RECONNECT_WINDOW_SECS` (60 s),
`SessionManager::on_disconnect` / `on_reconnect`, `ReconnectWindow`, and a
`MessageKind::ResumeHello` in the enum since minor 0. The desktop actor used
none of them. A lost link ended the session on both sides; the host moved it to
`Reconnecting` and then kept it there forever, and the guest either gave up or,
for a trusted host with a remembered password, waited out the window and asked
for a **new** session.

Three facts about the code shaped the decision:

- The id a guest knows is the one `HelloAck` carried, chosen per connection.
  `SessionManager` keeps an id of its own that never reaches the wire.
- A host decides admission in `on_handshaked`, right after `Hello`. Anything
  that arrives later arrives after a consent dialog may already be up.
- `ResumeHello` carries a session id and a sequence number, and nothing else:
  no features, no invite. The host needs the features again on a new
  connection, because every `speaks_*` set is per connection.

## Decision

### The claim is a `Hello`, not a `ResumeHello`

A resuming guest sends an ordinary `Hello`, with its features and its invite,
whose **envelope** `session_id` is the id of the connection that dropped. A
first connection sends all zeroes there, as it always has. `host_handshake`
reports a non-zero id as `HelloInfo::resume_claim` and still answers with a
fresh id.

This deviates from §10's wording, which names `ResumeHello`, and is recorded
here for that reason. Sending `ResumeHello` first would lose the features;
sending it after `Hello` would put the claim after the admission decision,
where a consent request has already been queued. The envelope field already
exists, is already covered by every golden vector, and an older host ignores
it and treats the connection as new.

`ResumeHello` stays in the enum unused, and the sequence numbers restart at 0
on the new connection. Frames cannot cross from the old QUIC connection to the
new one, so continuing the count would protect nothing.

### The host decides before the ticket and before anyone is asked

In `on_handshaked`, after the one-host-per-machine check and **before** the
ticket claim and `Admission`:

- A session that drops while granted is **parked** under the id of the
  connection it dropped on, and a timer fires when `RECONNECT_WINDOW_SECS`
  runs out. When it fires, the session is revoked, which also frees its place
  in the concurrent-guest limit. Before this change a `Reconnecting` session
  kept that place forever.
- A claim is honoured only when three things hold: the authenticated peer has
  a parked session, the ids match, and `SessionManager::on_reconnect` says the
  window is still open on the monotonic clock. When they do, the connection is
  adopted, the role and all grants come back unchanged (independent grants
  included), `ConsentGrant(role)` goes out, capture and announcements start
  again, and `SessionResumed { role }` is audited. `ConsentGranted` is not
  audited, because nobody granted anything.
- Any other claim closes the connection with the new `CLOSE_RESUME_REFUSED`
  (7) and ends any parked session of that peer. **It never becomes a consent
  request.** Otherwise any key holding a valid invite could raise a dialog by
  sending a made-up id, and a guest resuming unasked would do so to a host
  nobody told it could.
- The host may not have noticed the drop yet: a guest that lost its link can
  dial again before the host's QUIC connection times out. A claim that names a
  connection the host still holds is proof that connection is dead, because
  the same key cannot be on both. The host closes it, runs the ordinary
  teardown (which parks the session), and then decides.
- A plain `Hello` from a peer with a parked session ends that session first.
  A new session inherits nothing, and the old one cannot come back behind it.

What comes back is the session: role and grants. What lived on the connection
does not. Media restarts; tunnels, shells, file transfers, clipboard state and
chat are torn down at the drop, as they already were. Keeping them would mean
keeping per-connection state alive with no connection under it, which is the
failure ADR 0078 and ADR 0079 exist to prevent.

### The guest resumes first, for any host new enough

`PROTOCOL_MINOR` becomes 18 and adds no message. A guest makes a claim only to
a host whose `HelloAck` minor is at least 18, because an older host would read
the claim as a new connection and show a consent dialog.

When a watched session drops, the guest enters a new phase, `Resuming`, and
dials with the claim every `RESUME_RETRY_SECS` (3 s) while the window is open.
Unlike ADR 0084's wait, this happens for **any** host, trusted or not: a
resume never asks the host's user anything, and a refused claim raises
nothing.

- Success is the `ConsentGrant` that follows, which opens the view again as
  any grant does.
- A connection closed with `RESUME_REFUSED` is a refusal. The session is gone
  on the host, so the guest stops trying. For a host it may dial unasked it
  falls back to ADR 0084's wait, still not dialing a new session inside the
  window. For any other host it shows `SESSION_NOT_RESUMED`: "connect again".
- The refusal holds whether or not the guest read the host's `HelloAck`. The
  host closes right behind the ack, and a QUIC close discards stream data
  still in flight. A guest that lost the ack reads the refusal off the close
  code, not as a lost link it would dial again for. `CONSENT_UNAVAILABLE` is
  read the same way.
- A resume connection that drops for any other reason is not a refusal, and
  the next tick tries again.
- When the window ends without an answer, the guest falls back the same way.
- Cancel stops the attempt. Connecting again after that is a new request.

The UI shows `resuming` as its own panel, not as the restart wait. That panel
promises a password prompt, and this one promises that nobody will be asked.

## Consequences

- An ordinary network blip no longer costs a consent dialog or a device
  password. The session and its grants survive it, which is what §10 always
  said and what ADR 0084 assumed.
- A resume within the window restores grants without a person deciding
  again. That is §10's design, and it is bounded exactly as §10 bounds it:
  same authenticated key, same session id, 60 seconds on the monotonic clock.
  The host's session list does not show a parked session, so the person at
  the host cannot end one by hand during those 60 seconds. What they see is the
  session gone and then back.
- A session that never comes back stops holding a guest slot after 60 s.
- Tests (`crates/runtime/src/network.rs`):
  `a_dropped_link_resumes_the_session_with_its_grants_and_asks_nobody`,
  `a_resume_claim_for_no_session_is_refused_and_raises_nothing`,
  `a_claim_replaces_a_connection_the_host_has_not_seen_drop`,
  `connecting_again_instead_of_resuming_asks_for_consent`,
  `a_resume_refused_after_its_hello_ack_stops_resuming`,
  `a_resume_refused_before_its_hello_ack_arrives_stops_resuming`. In
  `crates/net`: `a_resume_claim_reaches_the_host_and_a_first_hello_carries_none`,
  `a_refusal_that_overtakes_hello_ack_is_still_a_refusal`. Golden
  vectors for minor 18 freeze a first `Hello` and a claiming one.

## Still open

- Checked only between actors on one machine. A real link loss across two
  networks, with the host's QUIC idle timeout in play, has not been tried.
- A file transfer still does not survive a control drop. The session now
  does, so this is only per-connection state that was not carried over, and
  it can be built on top of this change.
