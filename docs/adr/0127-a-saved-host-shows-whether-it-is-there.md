# ADR 0127 — A saved host shows whether it is there

Status: accepted
Date: 2026-09-28

Extends the remembered-hosts list of
[ADR 0016](0016-reusable-invites-and-guest-side-connection-history.md) and sits beside the
background lookup of
[ADR 0099](0099-a-saved-host-is-located-before-anybody-presses-connect.md).

## Report

The connection list showed every saved computer the same way whether it was
switched on or not. The only way to find out was to press Connect and watch a
dial run out its budget. People asked for an online/offline mark on each
saved computer: checked when the Lumepeer window is the active one, again
every five minutes only while it stays active, and updated by a connect too.

## Decision

### 1. A check is the first half of a connect

`history_probe` asks the actor to check every remembered host. For each row it
reads the saved code, verifies it as a connect does (ADR 0122), builds the
same dial plan (ADR 0083) and dials the control ALPN over each transport in
order — `TRANSPORT_PROBE_ATTEMPTS` attempts per transport, alternating ticket
addresses and lookup, `PRESENCE_ATTEMPT_TIMEOUT_SECS` (10 s) each. The first
finished QUIC handshake is the answer "there"; the connection is closed on the
spot with `CLOSE_PRESENCE` (8, `PRESENCE`) before any stream is opened. No
transport finishing one is "not there".

"Is it on" has no answer short of the machine answering, so unlike ADR 0099's
lookup this does reach the hosts. It reaches them the way a connect would —
the obfuscated transport knocks and punches (ADR 0113, ADR 0116) — which is
also why at most `PRESENCE_PROBES_AT_ONCE` (4) run at the same time.

A host this node is connected to, dialing, or waiting for is skipped: the live
connection or that dial's outcome is the better answer, and a second dial at
the same machine would race the first one's punch. A check already running for
a host is not started twice.

### 2. The host has nothing to decide

No `Hello` is ever sent, so the host never reads an invite, never queues a
consent request, never starts a session and audits nothing. Its handshake
task sees a connection that closed before the control stream opened; when the
close carries `CLOSE_PRESENCE` it logs that at debug instead of warning about a
failed handshake. A host older than this change warns, and is otherwise
unaffected.

### 3. A connect is a check too

Every dial outcome writes the same fact: a session, or any verdict from the
host (`is_verdict` — a refusal, an invalid ticket, a protocol it cannot speak),
means the host is there; a round of silence means it is not. While a
connection to the host is live, the row says "there" whatever was recorded
before.

### 4. Memory only, and blank until asked

The answer lives in the actor for this run and is never written to the history
file: an `online` read back after a restart would claim a machine is on because
it was on yesterday. `connection_history` carries it as `online: bool | null`,
`null` until something has asked, and the card keeps its grey "not connected"
dot for `null` — a red dot on a host nobody has checked would be a claim with
no evidence.

### 5. The window decides when, and only while it is in front

The main window asks (`PresenceSchedule`, `presence.ts`) when it comes to the
front — at most once per 30 s of switching back and forth — and again once the
last check is five minutes old, only while `document.hasFocus()` still holds.
It does not ask before this node has reached the network (`network_status`
`ready`), or every host would be painted off for a fault that is this
machine's own.

### 6. The mark

The dot the card already has beside the host's name (ADR 0121's card foot),
which until now was always the grey of "not connected": green when the host is
there, red when it is not, grey until something has asked. No halo — that is a
live session's dot — so "it is there" never reads as "you are connected". One
dot per card: a second one on the picture would have left the grey one beside
the name saying something else. The dot is colour only, so the card face's
accessible name says it in words too ("Connect again: …, Offline").

## Consequences

- A host is dialed every five minutes by each guest that keeps it saved and
  has its window in front. Each check costs what the first attempt of a
  connect costs; a guest with its window in the background costs nothing.
- An offline answer takes up to four attempts of 10 s on a two-transport plan;
  an online one usually arrives within a second or two.
- "Not there" also covers "this node cannot reach it": a firewall, a dead
  local network or a host whose saved code no longer verifies all look the
  same from here. A host whose code no longer parses or verifies keeps the grey
  dot.
