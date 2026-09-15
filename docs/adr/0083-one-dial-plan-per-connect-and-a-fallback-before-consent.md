# ADR 0083 — One dial plan per connect, and a fallback that happens before consent

Status: accepted
Date: 2026-09-14

Finishes what
[ADR 0080](0080-one-connection-type-over-two-transports.md) deferred in one
sentence — "trying one transport and falling back to the other, with the
diagnostics that needs, is deliberately not here" — and what
[ADR 0082](0082-hole-punching-without-a-signalling-channel.md) repeated when
it made the obfuscated dial fail quickly instead of hanging. The retry budget
it spends is [ADR 0050](0050-widening-the-dial-retry-budget-and-jittering-it.md)'s,
unchanged.

## Context

Two transports reach a host (ADR 0052, ADR 0080), and until now a guest used
exactly one of them: the obfuscated one if `[network] obfuscated` was on *and*
the ticket carried an address and a fingerprint, the iroh one otherwise. One
decision, taken before a packet was sent, with no way back. That is the right
shape while a transport is new and off by default, and the wrong one the
moment it is on: the obfuscated path has no relay and no address lookup, so
every network it cannot cross — a symmetric NAT, a restricted-cone host that
never sent first, two machines behind one non-hairpinning NAT, a Windows
firewall with no inbound rule (ADR 0082 lists them all) — was a failed connect
for a guest whose iroh path would have worked.

gap-tasks/23 asks for the fallback, and for three things about it that matter
more than the fallback itself: that the order is a decision written in one
place, that the user does not wait longer for it, and that it can never take
away a consent dialog the host has already shown.

## Decision

### 1. A dial is a *plan*: an ordered list of transports, built once

`Actor::dial_plan` builds it on the actor's own thread, before the task that
talks to the network starts, from `dial_order` — a pure function of two facts:
whether this invite can use the obfuscated transport at all, and which
transport last worked for this host. `dial_over_plan` then walks it.

**The order is obfuscated first, iroh last.** The obfuscated transport leads
because it is the one a user turned on deliberately and the one that gives up
quickly when it cannot work — ADR 0082 bounds its whole punch train at twelve
seconds. iroh is last because it is the transport that reaches a host no other
one can: it has a relay behind it and an address lookup behind that, and a
plan whose last entry can still fail is a plan that gives up early.

**The order gap-tasks/23 suggests as an example — iroh-direct, then
obfuscated, then iroh-relay — is not what this builds, and the reason is in
the code rather than in taste.** "Direct" and "relay" are not two dials on the
iroh transport. They are open paths on one connection, and which of them
carries a packet is iroh's own decision, taken per packet, with a direct path
already preferred (§5). The only switches this app has over that are
process-wide and taken at bind time — `LUMEPEER_RELAY_ONLY` /
`clear_ip_transports` (`crates/net/src/endpoint.rs`) — so expressing that
order would mean binding a second iroh endpoint per dial and holding a second
relay link for it, to split a choice iroh makes better with both halves in
hand. The plan therefore has one iroh entry, and the direct-before-relay
preference inside it stays where it already lives.

### 2. The attempts are divided between the transports, never multiplied

`DIAL_ATTEMPTS` (5) is the budget of ADR 0050 and it is unchanged. What
changes is that a plan *shares* it: every transport but the last gets
`TRANSPORT_PROBE_ATTEMPTS` (2, new), and the last gets the rest
(`attempt_shares`). Two rather than one because a single lost packet is not
evidence about a transport — the first attempt on a freshly bound obfuscated
endpoint races its own NAT mapping. Two rather than three because the
fallback is what actually connects the guest, and it should keep the larger
half.

Nothing else about the retry moves: the same `CONNECT_ATTEMPT_TIMEOUT_SECS`
bounds one attempt, the same jittered `DIAL_RETRY_BACKOFF_MS` +
`DIAL_RETRY_BACKOFF_JITTER_MS` spaces them, the alternation between the
ticket's addresses and an address lookup is untouched, and `dial_over_plan`
itself waits for nothing between transports. So the worst case is what it
already was, and `DIAL_TOTAL_BUDGET_SECS` — derived from those four constants
rather than chosen — is what a test holds it to. A two-transport plan is in
fact slightly *cheaper* than a one-transport one, because splitting five
attempts across two runs removes one of the backoffs between them. In
practice it is cheaper still: an obfuscated attempt that fails costs the
twelve seconds of ADR 0082's train, not the twenty a full attempt is allowed.

### 3. The fallback cannot happen after consent, structurally

A stage returns either a `ControlConnection` — the handshake of §9.1 is over,
the host has queued its consent request, the dial is finished — or an error,
in which case nothing this stage sent was ever answered. There is no third
outcome, so there is no state in which a guest is looking at a consent dialog
that a later transport switch takes away. This is a property of the control
flow rather than a check, which is why it is stated here instead of guarded
by one.

**An answer is never re-asked on another transport**, for the same reason ADR
0050 never retries one. A bad ticket, a version mismatch or a refusal is a
verdict about this guest, and the far side gives the same verdict however it
is reached; asking again over another transport only collects it twice. Both
the retry inside a transport and the fallback between transports read one
predicate, `is_retryable`, so the two cannot drift into "retried here but not
there".

The one window this does *not* close is one ADR 0050 already opened: an
attempt that runs out of its own budget a moment before the host finishes the
handshake leaves a consent request on the host that nobody is coming for.
Five sequential attempts could always do that; splitting them across two
transports does not add a case, and closing it needs the guest to withdraw a
request it can no longer see — a protocol change, and not this one.

### 4. What worked last time is remembered, and only reorders

A successful session records its transport in the remembered-hosts list
(`connection_history.rs`, whose path comes from `ActorStores` like every other
store), as `Option<TransportKind>` with `#[serde(default)]`, so a file written
before this field loads as "nothing to go on" — which is also what a host
nobody has dialed presents. The next dial to that host puts it first.

A memory only reorders what is available. It can never add a transport this
invite has no address for, and it never shortens the plan to one entry: a
host that moved onto a network where last time's answer no longer works still
gets the other transport, in the same dial, without the user doing anything.
A visit that cannot say which transport carried it — the row written when a
session ends, after the dialer is gone — leaves the remembered one alone
rather than forgetting it.

### 5. The transport does not change during a session, and a reconnect is a
new dial

`host_dialers` is written once, when the control channel comes up, and read by
every later channel (ADR 0080). Nothing replaces that entry for the life of
the session, so `rd/media/1`, `rd/file/1`, `rd/tunnel/1` and `rd/term/1` take
the transport the control channel took, and a fallback is over before the
first of them is opened.

When a session's transport drops, what happens next is what already happened:
the host moves the session into the reconnect window of §10
(`SessionManager::on_disconnect`, `RECONNECT_WINDOW_SECS`), and the guest
dials the invite again. That second dial builds its own plan and may well land
on the other transport. It changes nothing about the session it resumes,
because none of the three things §10 resumes by — the authenticated `NodeId`,
the `session_id`, the grants already given — is a function of how packets
arrive. A test asserts exactly that, because it is the kind of property that
is true until someone keys something by transport.

Recorded because it contradicts the task's wording, which describes the
desktop app as resuming through that window: in the code, the app never sends
`MessageKind::ResumeHello` and never calls `SessionManager::on_reconnect`.
`on_handshaked` gives every connection, first or second, a fresh
`request_consent_as` — "every connection, first time or reconnect, gets a
fresh decision". The window and the `ResumeHello` message exist in
`crates/core` and `crates/net`; the app has not been wired to them. That is a
gap of its own and not this pack's, and nothing here widens anything either
way: a re-dialed guest is asked for again, which is the stricter of the two
behaviours.

### 6. The panel says which transport, and what was given up on — nothing else

`ConnectionStats` carries the transport, read off the live connection
(`transport_of`) so the answer is the same on the side that dialed and the
side that accepted, and the fallbacks the dial recorded. The webview turns
both into words: an obfuscated session is named "obfuscated direct" rather
than "direct" (on that transport there is one direct UDP path and never a
relay, so the path row would be saying it twice), and each transport given up
on gets a row naming it and what this machine observed — nothing answered, or
the connection dropped.

**No guess about a cause reaches the panel.** The failure text comes from the
§18 code the error already carries, the same vocabulary the connect form
reads, and an unrecognised code gets a neutral phrase rather than its raw
text. This app cannot tell a network that blocks a transport from one that is
merely bad, and a panel that named the difference would be inventing it. The
error itself, the attempt count and the transport of every attempt are in the
log, where a technical answer belongs.

## Consequences

- With `[network] obfuscated` off — every shipping build — a plan has one
  transport in it, `attempt_shares` gives it all five attempts, and the dial
  is the one ADR 0050 describes, instruction for instruction. Nothing is
  bound, nothing is dialed, and no STUN request is sent, exactly as ADR 0080
  promised.
- A failure to *bind* the obfuscated endpoint is no longer a failed connect.
  It is a local failure — no socket — and it now leaves that transport out of
  the plan instead of denying the guest the iroh path it never needed a socket
  for.
- Two new constants in `crates/core/src/constants.rs`:
  `TRANSPORT_PROBE_ATTEMPTS` and the derived `DIAL_TOTAL_BUDGET_SECS`. No new
  dependency, no protocol change, no ticket-format change, no new grant, no
  new IPC command.
- `ConnectionStatsDto` grows two fields and the webview seven strings, in all
  thirteen locales. There is still no transport setting in the interface: the
  only thing a person chooses is whether the obfuscated transport may be used
  at all, which is the flag ADR 0080 added.
- The remembered-hosts file gains a field. An older build reading a newer file
  ignores it; a newer build reading an older one dials in the default order.

## Alternatives considered

- **Both transports at once, first one to finish wins.** Halves the worst
  case and doubles what a host sees: two QUIC handshakes from one guest for
  one session, two consent requests to race, and a host that has to withdraw
  one of them. The consent queue of §8.1 is the wrong place to learn about
  transports.
- **A setting for the order.** Forbidden by the task, and rightly: a user who
  can tell which transport their network allows does not need the app, and one
  who cannot would be answering a question about magicsock internals.
- **Falling back on a verdict too** ("maybe the other transport gets a
  different answer"). It does not: the ticket, the version and the host's
  refusal are all about this guest. It would double every refusal and hand a
  guest a second shot at a rate limiter.
- **Giving each transport its own full `DIAL_ATTEMPTS`.** The simplest code
  and the thing the task forbids — 108 seconds becomes 216, for a user who is
  already staring at a spinner.
- **Choosing by measurement instead of by memory** — probing both transports
  and keeping the faster. A probe that is not a session proves nothing about a
  session (ADR 0082 already argues this about punch packets), and a second
  shape on the wire is what this transport exists to avoid.

## Verification

- `cargo fmt --all -- --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace` — 593 passed, 0 failed, 1 ignored, 28 test
  binaries (585 before this pack). The eight new ones are the three about the
  order a plan comes out in, the one that divides the attempts, the one that
  holds the wall clock to `DIAL_TOTAL_BUDGET_SECS`, the one that says a
  reconnect over the other transport resumes the same session and widens no
  grant, and the two about what the remembered transport survives.
- `cd apps/desktop && npm run typecheck && npm test` — clean; 739 passed
  across 27 files (735 before; the four new ones are the panel's).
- **Not verified, and needing two machines behind two different NATs:** that
  the fallback actually rescues a session on a network where UDP or QUIC is
  cut, how long it takes there, and which transport wins on an ordinary
  network, a UDP-blocking one and a double NAT. One machine cannot answer any
  of it — the same limit ADR 0082 recorded for the punch itself. The run that
  would close it is in gap-tasks/23 task 4.
