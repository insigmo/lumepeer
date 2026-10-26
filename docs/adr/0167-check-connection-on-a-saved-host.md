# ADR 0167 — "Check connection" on a saved host

Status: accepted
Date: 2026-10-26

Builds on the presence check of
[ADR 0127](0127-a-saved-host-shows-whether-it-is-there.md).

## Report

The three-dot menu of a remembered host had no way to ask "can I reach this
machine right now, and how well" short of connecting to it. The owner asked
for a "Check connection" item that pings the other machine.

## Decision

### 1. One host, on request, answered to the caller

`history_check { peer }` checks one remembered host and answers when the check
is over with `{ outcome, rtt_ms, path, transport }`:

- `online` — a transport of the dial plan finished a QUIC handshake;
- `offline` — none did, after the same attempts a presence check makes
  (`TRANSPORT_PROBE_ATTEMPTS` per transport, `PRESENCE_ATTEMPT_TIMEOUT_SECS`
  each);
- `dialing` — this node is connecting to that host right now, so no check
  runs: that dial's outcome is the answer, and a second dial would race its
  punch (ADR 0127 §1).

A host this node holds a live connection to is answered from that connection
— its own `Ping`/`Pong` round trip, QUIC's estimate as the fallback — and is
not dialed again. What a check finds also becomes the row's `online`, so the
dot and the line under the card never disagree.

### 2. The round trip is QUIC's, not an echo

An application-level echo needs a session on the host, which means a `Hello`,
a consent request and an audit record — exactly what a check must not cause.
So the dial stops where a presence check stops, at the handshake, and is
closed with `CLOSE_PRESENCE`; the host logs it at debug and decides nothing.
The round trip reported is QUIC's own estimate of the path that answered,
which on a fresh connection is the handshake's sample.

On iroh, a handshake often lands on a relay first and moves to a direct path
when the punch lands. A check that read the first path would report a relay a
session leaves almost at once, so the connection is held for up to
`HOST_CHECK_SETTLE_MS` (2 s) while the selected path is still a relay — far
inside the host's `CONTROL_HANDSHAKE_TIMEOUT_SECS` wait for a `Hello`. The
obfuscated transport is one direct path and is read at once.

`path` names the path carrying packets (`direct`, `relay`, or `unknown`), never
`mixed`: the question is which way this check went, not which paths exist.

### 3. The card

"Check connection" sits after the three connect items. While it runs the card
says "Checking connection…" and the item is disabled; the answer stays on the
card for a minute (`HOST_CHECK_SHOWN_MS`), as "Online · 42 ms · Direct",
"Online · 120 ms · Via relay", "Online · 9 ms · Obfuscated direct", "Offline",
or that a connect is already under way. A check that could not run at all —
the row is gone, its code no longer verifies — says it could not check.

## Consequences

- An offline answer takes as long as a presence check's: up to four attempts
  of 10 s on a two-transport plan. The line says "Checking connection…" until
  then.
- One reading, not a series: a run of samples would mean a handshake per
  sample, each a knock on the host, for numbers the first one already gives.
- Unverified on a real pair; covered by runtime tests on two local endpoints
  and by the webview's unit tests.
