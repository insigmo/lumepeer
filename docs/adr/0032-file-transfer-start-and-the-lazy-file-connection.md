# ADR 0032 — `FileTransferStart` names a transfer, and `rd/file/1` opens only once one exists

Status: accepted
Date: 2026-08-27
Extends: §9.2 (file transfer), §4 / §4.1 (three connections, one per ALPN),
§8.2 (grants), §2.3 (only the host's core authorizes), §15 (what is never
recorded), §18 (degrade, and say so)
Builds on: ADR 0029 (the `file_transfer` grant became issuable), ADR 0027
(blocking work leaves the actor loop), ADR 0026 (the guest is the side that
dials)

## Context

`crates/net/src/file_transfer.rs` was a finished, tested chunk engine:
sequential accounting, a running BLAKE3, `FILE_CHUNK_MAX_BYTES` checked before
allocation, `MAX_CONCURRENT_FILE_TRANSFERS`, and a resume point that is the
last acked offset. `FileOffer`, `FileAccept`, `FileAbort` and `FileChunkAck`
were in the protocol. `ALPN_FILE` was declared and `Channel::File` was
recognized.

None of it was reachable. `grep -rn file_transfer crates apps --include=*.rs`
found no reference outside the module itself. An incoming `rd/file/1`
connection was closed unconditionally, no side ever dialed one, and there were
no IPC commands and no UI.

There was also a hole in the protocol. `FileAbort` and `FileChunkAck` are both
documented as naming a `transfer_id` "announced in `FileTransferStart`" — and
`FileTransferStart` did not exist. `FileOffer` carries no identifier either, so
the two sides had no way to agree on what an abort or an ack referred to. The
engine's whole vocabulary was ids, and nothing issued one.

## Decision

### The sender names the transfer, in a message that restates the offer

`MessageKind::FileTransferStart { transfer_id, name, size, hash }` is appended
at the end of the enum (discriminant 36), `PROTOCOL_MINOR` becomes 5, and it
rides behind `FEATURE_FILE_TRANSFER`. The sender sends it after
`FileAccept(true)` and before the first chunk.

The alternative was to derive the id from the offer's content. That was
rejected: `MAX_PENDING_FILE_OFFERS` is 3, the same file may legitimately be
offered twice, and a content-derived id would then collide with a transfer
already running under it.

Restating `name`, `size` and `hash` is the part worth defending, because it
looks like duplication. It is not. `FileAccept` is a bare boolean: it does not
say which offer it answers, so an id that meant "whichever offer we both
believe was accepted last" would be shared state with nothing to check itself
against. The receiver now compares the start against the offer it actually
agreed to and aborts on any mismatch — a sender cannot start file B under an
answer given for file A. Three fields buy an invariant the receiver can
enforce alone.

The receiver takes the id and nothing else from it: `ReceiveTracker::begin_with`
refuses an id that already has bytes and a running hash behind it, so a
repeated start cannot reset a transfer in flight.

### A guest learns the host's minor; a host reads the guest's features

Feature strings only travel one way. A guest advertises what it understands in
`Hello`; `HelloAck` carries no feature list at all. Since either side may offer
a file, the guest needs the same knowledge — so `ControlConnection` now keeps
the far side's `PROTOCOL_MINOR`, and the guest gates on `minor >= 5`. §9.1
makes that sound: a peer at minor N decodes every optional message added at or
below N. Offering a file to a peer that could not decode the start is refused
up front with a distinct `PEER_TOO_OLD` code, rather than started and then
found unackable (§18).

### `rd/file/1` opens after an acceptance, and only the guest dials it

§4 requires that neither a media stream nor a large transfer can delay a
revoke on the control channel. The connection is therefore opened at exactly
one moment: when an offer has been accepted by a peer whose session holds
`file_transfer`. A declined offer opens nothing at all — not "opens and closes",
which is the property the integration test asserts by watching the accept side
stay silent.

Only the node that dialed the control connection dials this one, exactly as
with `rd/media/1`: the host was dialed and holds no address for the guest
(ADR 0026). So on the host the file connection arrives, and a send queued
before it lands waits for it rather than failing.

The old unconditional close in `classify_incoming` had a comment worth keeping:
"an unauthenticated peer must not be able to park a file connection in the
control handshake's read". That invariant survives, and the comment now says
how. The ALPN is decided before any read; the file arm never runs the control
handshake; and the actor — the one place that can read `SessionManager` —
closes the connection immediately unless a live granted session with
`file_transfer` already exists. Authentication happens in the accept task,
authorization on the actor's own thread, which is the same split media has.

A revoke, a disconnect or a view window closing takes the file connection with
it, cancels every tracker for that peer and deletes every staging file. Nothing
is exported on the way out: a transfer still running when the grant behind it
ended is one that never finished being allowed.

### Staging lives beside the destination

`StagedReceive` writes to `.lumepeer-<id>.part` in the directory the receiving
user chose, and `export` renames it into place once — and only once —
`ReceiveTracker::finish` has matched the offer's BLAKE3.

Beside the destination rather than in a staging root, for two reasons. The
export becomes a rename on one volume instead of a second pass over up to 500
MiB. And a destination that turns out to be unwritable fails at the first
chunk instead of after the last one.

Nothing reaches the destination name before the hash matches. A file under the
name someone was expecting, which is not the file they were expecting, is
worse than no file: nothing about it announces that it is wrong.

### An offered name is refused, never repaired

`safe_file_name` accepts a single ordinary path component and nothing else.
`../../etc/passwd`, `..\windows\...`, absolute paths, `.`, `..`, a Windows
drive-relative `C:report.pdf`, an NTFS alternate data stream `notes.txt:secret`,
the reserved device names (`CON`, `LPT9.log`, `nul`), trailing dots and spaces
that Windows silently strips, control characters, and anything over
`FILE_NAME_MAX_BYTES` — all rejected.

Rejected rather than sanitized, deliberately. Rewriting a hostile name produces
a file the receiving user did not agree to, under a name neither side chose.
Refusing produces a question they can answer. The check runs on the sending
side too, so a file this machine cannot name safely is never offered at all
rather than being declined for reasons the sender cannot see.

`FILE_NAME_MAX_BYTES` (255) is new, and now bounds `FileOffer` as well, which
had only ever been bounded on its size. 255 is the per-component limit of every
filesystem this ships on, so a longer name could not be written down anyway.

### The receiver picks the destination; the sender picks a name

Both pickers — the file to send, the directory to receive into — run in Rust,
through `tauri-plugin-dialog`, invoked from inside the application's own IPC
commands. No `dialog:` or `fs:` permission appears in any capability file, so
the webview cannot open either one. `capabilities/view.json` states that a view
window has no filesystem rights; a picker invoked from the webview would be
that right wearing a different name (§2.3).

What crosses the IPC boundary is a peer label, a basename and a byte count. A
path on this machine never does, in either direction (§15).

Dismissing a picker is a success that offered nothing — not a decline. An
accept dialog the user closed has not answered the offer, and answering it for
them would be the panel making a decision.

### Completion is the receiver's word, and it means "verified and on disk"

The receiver acks contiguous bytes as they land, which is what a resuming
sender picks up from (§10). The ack *at the full size* is withheld until the
hash has matched and the export has succeeded. So "the sender saw `size`" and
"the file is on the receiving disk" cannot come apart, and a sender never
reports a completion only the receiver could know about. A hash mismatch or a
failed export sends `FileAbort` instead.

### Nothing large runs on the actor loop

Hashing a file for an offer is a full disk pass and runs on its own task
(ADR 0027). Each transfer gets its own unidirectional stream and its own task,
so three concurrent transfers do not serialize behind the slowest of them. A
256 KiB chunk is written to staging under the peer's own lock and only a byte
count travels back through the mailbox; progress uses `try_send`, because a
dropped progress update costs a UI frame and blocking a transfer on the actor's
mailbox would cost the transfer.

One consequence of two connections: nothing orders `FileTransferStart` against
the first chunk, because they arrive on different QUIC connections. A stream
reader that meets an unknown id waits on a `watch` signal (edge-triggered, so a
start landing between the check and the wait cannot be lost) for
`FILE_TRANSFER_START_TIMEOUT_SECS`, then aborts that stream — one transfer, not
the connection.

### The audit records the action, never the file

`AuditEvent::FileAction { action }` carries a short tag and the pseudonymized
peer label. No file name, no size, no path (§15). The panel shows names, since
it is answering "should I take this file?", but nothing about a file reaches a
log line or the notification bus — `ActorNotification::FileTransferChanged` is
a bare variant, and the UI polls for the detail.

## Consequences

- File transfer works end to end for the first time, gated per session on a
  grant a host user has to turn on.
- `PROTOCOL_MINOR` 5. Every earlier discriminant is unmoved and every frozen
  golden vector still passes; three new vectors are appended.
- New dependency `tauri-plugin-dialog`, which pulls `tauri-plugin-fs` as a
  library. That plugin is never registered, so its commands do not exist in
  this app, and no capability names them.
- `tokio`'s `fs` feature is enabled workspace-wide. A 500 MiB transfer written
  through blocking `std::fs` from inside an async task would stall a runtime
  thread per chunk.
- Resume is implemented and proven where it lives — in the engine and over a
  real dropped file connection in the integration test. The desktop actor
  still ends a session when its *control* connection drops (§10's reconnect
  window is not wired into the actor by any feature yet), so a resume there is
  a resume of the file channel under a control session that never went away.
  Wiring the control-side window is out of scope here and stays open.
- A guest whose offer the host declines for lack of a grant sees a decline and
  no reason, because `FileAccept(false)` carries none. Adding one would be a
  second statement of a grant that ADR 0029 keeps off the wire.

## Verification

- `crates/net`: every hostile name shape refused and every ordinary one passed
  through unchanged; a transfer id taken once; staging exported on a matching
  hash and nothing left behind on a cancel; a full-but-corrupted transfer not
  exported; a resumed send continuing from the acked offset with no gap; a
  chunked file hash equal to the one-shot hash.
- `crates/core`: `FileTransferStart` roundtrips, is bounded on both the size
  and the name it repeats, and sits at discriminant 36 with every earlier one
  unmoved.
- `tests/integration/tests/file_transfer.rs`: the whole cycle over real
  connections; a declined offer opening no connection at all; a cancel leaving
  neither staging nor a destination file; a dropped file connection resuming
  from the last ack and delivering the exact bytes; an offer refused without
  the grant and taken once the host turns it on — under `FullControl`, so that
  what refuses it is `Grants::file_transfer` and not the role.
- `file-transfers.test.ts`: an offer is never taken without a press, the answer
  carries the direction the button says, only this session's offers are shown,
  a running transfer can be cancelled by its own id, an ended one shows an
  outcome instead of a cancel button, the send button supplies no path, and the
  panel does not exist for a session without the grant.
