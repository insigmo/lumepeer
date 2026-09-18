# ADR 0091 — A resume is paced like a resume

Status: accepted
Date: 2026-09-18

Answers "after the VPN comes up the connection drops; it should survive the
internet going away", and corrects
[ADR 0089](0089-a-dropped-session-resumes-inside-its-window.md), whose
"the guest dials with the claim every `RESUME_RETRY_SECS` while the window is
open" was not what the code did.

## Context

ADR 0089 built session resume and bounded it at §10's 60 seconds, with the
guest re-dialing every `RESUME_RETRY_SECS` (3 s). Its own `const_assert` said
so:

```rust
const _: () = assert!(
    RESUME_RETRY_SECS * 3 < RECONNECT_WINDOW_SECS,
    "a resume must get several attempts inside the reconnect window"
);
```

That assertion held and the property did not, because the timer is not what
paces a resume. `resume_tick` will not dial while one is already in flight,
and a resume dial ran on the **first-connection** budget of
[ADR 0050](0050-a-dial-retries-because-a-relay-link-flaps.md): `DIAL_ATTEMPTS`
(5) attempts at `CONNECT_ATTEMPT_TIMEOUT_SECS` (20 s) each, which is
`DIAL_TOTAL_BUDGET_SECS` — **109 seconds, against a 60-second window**.

A guest log of a real drop, from this machine:

```text
07:18:38  waiting for a host to come back  first_attempt_secs=3 resuming=true
07:18:41  dialing the host                 plan=[Iroh]
07:19:01  connect attempt failed  error="dial failed: no answer within 20s"  attempt=1 of 5
07:19:22  connect attempt failed  error="dial failed: no answer within 20s"  attempt=2 of 5
07:19:44  connect attempt failed  error="dial failed: no answer within 20s"  attempt=3 of 5
```

The window closed at 07:19:38, underneath attempt 3 of one dial. A resume got
one try. Three seconds was never the cadence; 109 seconds was.

The second half of the report is not a bug at all but a bound that was too
small for what it is now asked to survive. An interface change is not instant
on either side: the machine whose network moved has to notice it, rebind its
sockets, re-probe a relay and republish its address before anything can reach
it, and a minute is an ordinary time for that. Longer still when the link is
simply gone for a while, which is what a person means by "the internet
dropped".

## Decision

### A dial has a pace, and a resume's is not a first connection's

`DialPace` carries two numbers — how long one attempt may take, and how many
attempts a transport may spend — and `dial_over_plan` picks it from one fact:
whether the dial carries a resume claim.

- **Connecting** keeps ADR 0050 exactly: `CONNECT_ATTEMPT_TIMEOUT_SECS` per
  attempt, the plan's own share of `DIAL_ATTEMPTS`. A person is waiting at a
  form, and the only thing worse than waiting is being told "could not
  connect" while the host was merely between relays.
- **Resuming** is `RESUME_ATTEMPTS` (2) attempts of
  `RESUME_ATTEMPT_TIMEOUT_SECS` (5 s). Nobody is waiting at a form: the wait's
  own tick is the retry loop, so an attempt that holds on is not persistence,
  it is the reason the next attempt never happens.

Two attempts rather than one because `dial_with_retries` alternates its route:
the odd attempt dials the addresses the ticket carries, the even one asks
discovery where the host is *now*. After the kind of network change a resume
exists for, the second is the one that can work, so a one-attempt dial would
never take it.

Worst case a resume dial now costs `RESUME_DIAL_BUDGET_SECS` — about twelve
seconds — so the 3-second tick produces a dial roughly every twelve seconds
instead of one per window.

### The assertion is restated against what actually paces a resume

```rust
const _: () = assert!(
    RESUME_DIAL_BUDGET_SECS * 4 < RECONNECT_WINDOW_SECS,
    "a resume must get several dials inside the reconnect window"
);
```

Against the cost of a whole dial, not against the timer, because the tick
cannot ask again while a dial is in flight. That is the difference between an
assertion that describes the behaviour and one that passes while the
behaviour is false.

### The window is 300 seconds

`RECONNECT_WINDOW_SECS` goes from 60 to 300.

What the window bounds is how long a session's **grants** may come back
without anybody being asked again. Widening it widens nothing about *who* may
come back: a resume still needs the same authenticated key, the same session
id, and a monotonic clock that cannot be wound back (§10, §12.3; ADR 0089).
What it costs is that a guest slot stays held, and that the person at the host
sees the session gone and then back, for up to five minutes instead of up to
one. It stays well below `REBOOT_WAIT_CEILING_SECS` (600 s), so a machine that
is actually gone still ends its sessions, and the `const_assert` that keeps
those two apart is unchanged.

## Consequences

- A link that comes back inside five minutes gets a resume roughly every
  twelve seconds for the whole of it — around twenty-five dials, fifty
  attempts, half of them through discovery — instead of one dial that outlived
  its own window.
- A VPN coming up, an interface changing, or the internet going away for a
  couple of minutes costs the picture and nothing else. No consent dialog, no
  device password, and the grants come back as they were.
- A first connection is unchanged in every respect. `DIAL_TOTAL_BUDGET_SECS`,
  the attempt shares of
  [ADR 0083](0083-one-dial-plan-per-connect-and-a-fallback-before-consent.md)
  and the test that holds a plan to one transport's cost all still apply to it
  and are untouched.
- Test (`crates/runtime/src/network.rs`):
  `a_resume_dial_fits_inside_its_window_several_times_over`, which asserts the
  pace follows the claim, that a resume spends its own attempts rather than
  the plan's share, that it takes both routes, and that the whole dial fits
  inside the window four times over — the property ADR 0089 assumed.

## Still open

- The view window still closes at the drop and a new one opens when the
  resume lands. That is ADR 0089's behaviour and it reads, to the operator, as
  a connection that dropped even when the session came back three seconds
  later. Keeping the window up under a "reconnecting" overlay is a better
  answer and is not this change.
- Verified between two actors and against the log above, not against a second
  real link loss. The numbers here were chosen from one measured failure; a
  second one is what would show whether five seconds per attempt is enough on
  a slow relay.
- A file transfer still does not survive a control drop, as ADR 0089 left it.
