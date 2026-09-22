# ADR 0106 — A session that dropped is dialed again at every host

Status: accepted
Date: 2026-09-22

Answers `docs/bugs/22-connection-survives-the-network.md` defect C, in the
shape its task 3 offered as variant 2 and the user chose. Narrows
[ADR 0084 §6](0084-restarting-the-host-is-its-own-grant-and-a-warned-act.md)
and the rule it cites from ADR 0044. Sits on top of
[ADR 0105](0105-a-view-window-outlives-the-connection-it-was-opened-for.md),
which is what makes the longer wait something the user can watch rather than
something that happens behind a closed window.

## Context

A guest whose link goes away gets `RECONNECT_WINDOW_SECS` — five minutes —
in which the same session comes back with its grants and nobody is asked
(§10; [ADR 0089](0089-a-dropped-session-resumes-inside-its-window.md)). That
is the right rule for what it covers.

After those five minutes, and immediately when a host refuses the claim, the
code asked a different question: `may_auto_reconnect`, which is
`history.is_trusted(tag) && remembered_password(tag)`. A host that failed it —
which is nearly every host, because the trusted flag is off until somebody
turns it on — got `ConnectPhase::Failed`, the code `SESSION_NOT_RESUMED`, and
a button.

So "the internet was out for ten minutes" ended at a button, and the user's
comparison — «так умеют другие аналоги и все ок» — is with products that keep
trying.

The reason for the gate is ADR 0044's, and it is a good one: *an attempt that
ends at a prompt nobody is there to answer is a dial loop rather than a
reconnection.* It was written for ADR 0084's case, a host this node had just
rebooted, where the point was to let itself back in unattended.

That is not this case. Here a session the user was sitting in lost its link,
and since ADR 0105 they are looking at its window with a banner on it. There
is somebody at the prompt. There is somebody at the other prompt too — the
person at the host, who pressed "allow" ten minutes ago and would press it
again.

## Decision

### 1. Every session this node was in is waited for

`on_disconnect` arms a `ReconnectWait` whenever `was_watching`, with no
second condition. `on_resume_refused` and `resume_tick`'s expiry both hand the
wait on to ADR 0084's dial instead of ending it. `on_reconnect_wait_tick` no
longer re-reads the trusted flag.

The wait's own bounds are untouched: `REBOOT_WAIT_RETRY_SECS` between
attempts, `REBOOT_WAIT_CEILING_SECS` (ten minutes) in total, after which
`HOST_DID_NOT_RETURN` and the button. No constant in
`crates/core/src/constants.rs` changed, and in particular
`RECONNECT_WINDOW_SECS` did not: variant 1 of the bug document's task 3 —
simply making the silent window longer — was **not** taken, because what that
window bounds is how long grants come back *without anybody being asked*, and
that is §10's line rather than a number about patience.

### 2. What comes back is a new session, and somebody is asked for it

This is the whole of why §1 is allowed, and it is ADR 0084's own sentence,
unchanged: **what comes back is a new session.** New consent, or a new device
password — never the grants the old one held. The wait dials; the host decides
exactly as it decides for a stranger with a valid invite; the guest gets
whatever that decision gives it.

Nothing in §10's resume is widened. A resume claim is still only made inside
`RECONNECT_WINDOW_SECS`, still only for the same session id, same key,
monotonic clock. Past the window there is no claim — only an ordinary connect
that happens to have been started by a timer rather than by a button.

### 3. The trusted flag now decides silence, not dialing

`may_auto_reconnect` is renamed `may_return_without_asking` and read in
exactly one place: the `UnattendedChallenge` handler, and there only when the
connect it belongs to was started by a wait rather than by the user.

- **Marked "reconnect on its own", password remembered** — the challenge is
  answered from the keystore and nobody is asked anything. Exactly as before.
- **Not marked** — the wait still dials, the challenge still arrives, and the
  person at this keyboard answers it. Or the host asks its own person for
  consent and they answer that.

This is what the address book has always promised in words: "come back to it
without being asked". The switch keeps that meaning and loses one it was never
labelled with. `auto_reconnect_allowed`'s truth table is unchanged and still
tested; only what failing it costs has changed.

A consequence: withdrawing the flag mid-wait no longer stops the wait.
It could not sensibly: the flag is not why the wait is running, and taking
somebody's window down (ADR 0105) because they unticked a box about silence
would be a surprise.

### 4. A host that answered ends the wait

Dialing every fifteen seconds for ten minutes is only acceptable while nobody
has said no. Two guards, both reading the rule ADR 0100 already wrote down:

- `on_reconnect_wait_tick` stops when the connect form holds a verdict —
  `ConnectPhase::Denied`, `ConnectPhase::Failed`, or any `connect_failure`.
  A refused consent, a refused device password, a host that cannot decide.
- `on_dial_failed` stops when `is_verdict(error)`, for the answers that
  arrive as a failed dial rather than as a message: an invalid or expired
  ticket, a protocol version this node cannot speak, `ConsentUnavailable`.

A host nobody is at is not a verdict and never was: the consent dialog stands,
the connection stays open, and the tick's existing "a connection is already
open" branch means no second dial is made while it does.

## Consequences

- The network can be gone for ten minutes and the session comes back by
  itself, at any host, with the host's person pressing "allow" once. Which is
  what the complaint asked for.
- A host that says no is asked once, not forty times. That is new — the old
  code could loop at a trusted host too, and nobody had noticed because the
  trusted path auto-submits a password and rarely reaches a Deny.
- `SESSION_NOT_RESUMED` is now raised in one case only: a refused resume whose
  history row has been removed in the meantime, so there is no invite left to
  dial with. Its wording — "Connect again to ask for a new one" — is exactly
  right there and would have been wrong anywhere else.
- The two ADR 0089 refusal tests changed what they assert, from
  `Failed`/`SESSION_NOT_RESUMED` to `WaitingForHost` with no failure code.
  Their names still hold: a refused resume stops *resuming*. What follows it
  is no longer nothing.
- **Not covered by an automated test:** the dial that happens after
  `RECONNECT_WINDOW_SECS` elapses, at an untrusted host, and the consent it
  asks for. Reaching it takes five minutes of wall clock, and these tests run
  real sockets on a multi-thread runtime, so tokio's paused clock is not
  available to shorten it. It is on the bug document's manual checklist.
- A guest slot on the host is not held by any of this: the host's own
  reconnect window is `RECONNECT_WINDOW_SECS` and is unchanged, so a host
  whose guest is in the ten-minute wait has released everything five minutes
  in and sees an ordinary new request when the dial lands.

## Alternatives considered

- **Variant 1 — raise `RECONNECT_WINDOW_SECS`.** The cheapest change and the
  wrong one. That window is how long a session's grants come back *with nobody
  asked*, bounded by §10 and by a `const_assert` against
  `REBOOT_WAIT_CEILING_SECS`. Stretching it to cover "the internet was out"
  would be widening a silent-authorization window to solve a patience problem,
  and it would hold a guest slot on the host for the whole of it.
- **Variant 3 — both.** Same objection as variant 1, plus the same benefit
  twice: once the wait continues at every host, the extra minutes of silence
  buy nothing the loud path does not already buy.
- **Keep the gate but add a "keep trying" button.** A button the user presses
  while looking at a window that says "reconnecting" is a button asking them
  to confirm what they are already watching.
- **Dial immediately after a refused resume instead of waiting out the rest
  of the window.** A `RESUME_REFUSED` proves the host is reachable *now*, so
  the remaining minutes of the window are spent waiting at a machine already
  known to be up. It is a real improvement and it is a change to ADR 0089's
  window, not to this one — `on_reconnect_wait_tick` deliberately refuses to
  raise a new session inside that window. Left for its own decision, with the
  observation written into the bug document.
