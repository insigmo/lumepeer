# ADR 0016 — Reusable invites, guest-side connection history

Status: accepted
Date: 2026-08-19

## Context

Three complaints from real two-machine use, all one knot:

1. Sending a connect request twice produced a run of errors. §7 makes an
   invite single-use, so the second attempt with the same code was refused by
   `TicketRegistry::claim` — and worse, the guest's second dial replaced the
   live entry in the actor's `connections` map, so the failed attempt tore
   down the session that was working.
2. The connect form went back to enabled the moment `invite_connect` resolved,
   which is when the *handshake* landed, not when the host user decided. There
   was nothing to stop a second request, and nothing on screen said a decision
   was outstanding.
3. Ended sessions were recorded on the host — the side that was dialed. The
   side that would want the list, the one that chose to dial, kept nothing.

The last one is the interesting one. A remembered-hosts list only earns its
place if a row can actually dial the host again, and with a single-use,
10-minute invite every row is a dead link within minutes of being written.

## Decision

### 1. An invite is live until it expires or the host replaces it

`TicketState` becomes `Live | Expired | Retired`. `claim` refuses an invite
this host never issued, one past its TTL, and one the host has retired; a
repeat claim of a live invite is accepted. `TicketRegistry::retire_all` is
called when the host issues a new invite, so exactly one code is live at a
time and "Refresh invite" is the host's withdrawal switch.

This deviates from §7's "single-use". The argument for single-use is that a
leaked code should not be a standing door — but it never was the door.
Consent is: the host is asked, by name, on every single connection, and a
guest holding a valid code still gets nothing until someone clicks. What
single-use actually bought was that a *legitimate* guest could not come back,
which is the behaviour the punch list is asking to remove. The bounds that
remain are real: a 10-minute TTL, a signature over the host's address, and a
host-side switch that kills the code immediately.

`consume()` is gone. It was never called — the handshake path only ever
claimed — and under this model there is nothing for it to mean.

### 2. Connection history belongs to the side that dialed

`ConnectionHistory` moves from the host to the guest. `stop_view` is the one
place every way a dialed session can end passes through (host revoke, dropped
transport, operator closing the window), so that is where a row is written.
The host records nothing at all: it decided once, the decision ended with the
session, and a roster of who visited is a record it never asked to keep.

Two things follow from wanting rows that still work tomorrow:

- **The label has to survive a restart.** `peer_tag`'s install salt is
  regenerated every start precisely so a guest cannot be correlated across
  runs — right for someone else's identity in this host's UI, wrong for the
  list of hosts *this* user picked. `host_tag` hashes the `NodeId` under a
  fixed domain-separation salt instead. Still one-way, still no raw `NodeId`
  on disk or in the webview, but stable.
- **The row keeps the invite code.** It stays in Rust: `connection_history`
  hands the webview a label and nothing else, and `history_connect` looks the
  code up actor-side. The webview names a host; it never holds the means to
  reach one it was not already given.

One row per host, not per session. This is a list of places to go back to.

### 3. The connect form waits on the host's decision, not on the dial

A new `connect_status` command reports a guest-side `ConnectPhase`
(`idle | awaiting_consent | connected | denied | failed`). `connect_with_ticket`
sets `awaiting_consent`; `ConsentGrant` moves it to `connected`, a
`ConsentRevoke` on a request that was never granted to `denied`, and a dropped
connection to `failed`. The button stays disabled for the whole of
`awaiting_consent`, and "Connecting…" animates through it.

`connect_with_ticket` also refuses to dial a host this node already holds a
control connection to (`NetError::AlreadyConnected`), which is what makes a
second Connect harmless rather than destructive.

A guest that receives `ConsentRevoke` now drops its control connection as
well. With the grant withdrawn there is nothing left to say on it, and leaving
it open both held a stream on the host for a session that no longer existed
and made the next Connect to that host look like the duplicate above.

## Consequences

- A leaked invite code is usable until its TTL runs out or the host refreshes
  it, rather than exactly once. Every use still faces consent.
- The guest's `connection_history.json` now contains invite codes. It sits in
  the app's local data directory with the same exposure as the keystore-backed
  identity beside it; anyone who can read it could dial those hosts and would
  still be refused at the consent dialog.
- §7 and §14's `INVITE_TICKET_TTL_SECS` are unchanged in value. Ten minutes is
  short for a remembered-hosts list — a row goes stale once the host's invite
  expires, and the operator has to be sent a new code. Whether the sidebar
  code should instead be a long-lived device address is a product decision
  this ADR deliberately does not take.
- Nothing in `crates/core` changed. Authorization still lives entirely in the
  consent state machine; this ADR only changes what an invite means before
  that machine is reached, and which side keeps a list afterwards.
