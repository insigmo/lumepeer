# ADR 0077 — A directory is a manifest, and the ceiling is free space

Status: accepted
Date: 2026-09-10

Extends [ADR 0032](0032-file-transfer-start-and-the-lazy-file-connection.md),
whose engine every file here still moves on, and
[ADR 0076](0076-naming-a-file-on-the-host-is-browse-plus-transfer.md), whose
window is where a directory is named. Uses the path parser of
[ADR 0075](0075-listing-a-directory-is-its-own-grant.md) for the one check
that matters.

## Context

Three things were left over after ADR 0076, and they are one problem seen
from three sides.

**500 MiB.** `FILE_OFFER_MAX_BYTES` was a round number with no reasoning
attached. Nothing downstream allocated it — chunks are bounded and checked
before a byte is read, the hash is streaming, the receiver writes to staging —
so it was not a protection. It was a policy, and the policy said no to a disk
image, a video, a database dump and most of what a person reaches for a remote
desktop to move.

**"No room" at ninety per cent.** Nothing asked whether the file would fit
before accepting it. A full volume was discovered by a failing write, after
the sender had spent the time and the receiver had a staging file to throw
away.

**A directory could not be sent at all.** The protocol had one message for one
file, and a folder is the unit people actually move.

There was also a claim to check. The resume point of §10 was written down as
working: "a reconnecting sender starts from the last acked offset, never from
zero". The receiver's half was real — `ReceiveTracker` keeps the contiguous
byte count, refuses any chunk that is not at it, and acks it as it goes — but
`SendJob::from` was constructed with `0` at every one of its call sites and
nothing ever read a resume point back. **It worked in the receiver and was
never asked for by the sender.**

## Decision

### The ceiling is 64 GiB, and the real one is free space

`FILE_OFFER_MAX_BYTES` becomes 64 GiB and says in its own doc comment that it
is a policy about what "a file someone meant to send" means, not a bound
anything allocates. `FILE_OFFER_LEGACY_MAX_BYTES` freezes the old value as
what a peer below `PROTOCOL_MINOR` 14 accepts, and a sender does not make an
offer above it to such a peer — read off the peer's announced minor, in both
directions, because both sides announce one and a feature string would only be
needed if one of them did not.

This is **the one verdict in `tests/interop/golden_vectors.txt` that has ever
changed**, and the bytes did not: `file_offer_over_the_minor13_size_limit`
still holds the same frame, and today it decodes. That is written down in the
vectors file itself and in the golden test's own header, because an interop
partner replaying it against an older build still gets the old answer, and
that is exactly what the minor gate is for.

Before any accept, the receiver asks whether the file fits where it is going —
`disk::free_space_bytes`, minus `STAGING_FREE_SPACE_MARGIN_BYTES` so a volume
is never filled to its last byte by a transfer. A volume that will not answer
is treated as room: refusing a transfer because the operating system declined
to say would be a worse failure than the one this prevents. The refusal is
`ActorError::NoSpace`, which the IPC layer turns into a `NO_SPACE` code the
window can put a sentence to.

`FILE_CHUNK_MAX_BYTES` does not move. A chunk's size has nothing to do with a
file's.

### A directory is a manifest of files

`PROTOCOL_MINOR` 14 appends `DirOffer { name, dir, entries }` and
`DirAccept(bool)` behind `FEATURE_DIR_TRANSFER`. The sender walks the tree
once and names every file in it with its size; the receiver takes the whole
manifest or none of it; each file then moves as an ordinary transfer, with its
own `FileTransferStart`, its own BLAKE3 and its own staging file.

Not an archive. Archiving would be one transfer instead of many, and it would
cost the resume point of every file after the first, hide the contents from
the check the receiver makes before it accepts, and add a compression
dependency to a process that authorizes remote input.

- **Empty directories are entries**, with `is_dir` set. Every other directory
  is implied by the paths of the things inside it; an empty one is implied by
  nothing, and a tree that arrives without its empty folders is not the tree
  that was sent.
- **Symbolic links are skipped and counted.** Following one would offer a file
  outside the directory the sender picked; recreating one would be a claim
  about a filesystem this side cannot see. The count goes in the log.
- **Every entry path is checked by `remote_path::safe_relative_path`** — no
  `..`, nothing absolute, no drive letter, no empty component, and each
  component through the same rules a file name goes through. This is where
  "zip slip" would be, so the check has a function of its own,
  `manifest_destination`, which is the only place a manifest entry becomes a
  path on this machine.
- **`MAX_DIR_MANIFEST_ENTRIES` is 200**, because a manifest is one control
  frame and a control frame is 64 KiB — asserted at compile time against
  `MANIFEST_PATH_MAX_BYTES`. A tree with more files in it is refused out loud
  rather than truncated into one that arrives incomplete.
- **The window sees one row.** Progress is the sum of the files, the state is
  the worst of them, and a cancel takes all of them: a tree half moved is not
  half a result. The members stay in the actor's own map, because that is what
  the engine acks and aborts by.
- **Hashes are computed at accept time, not at walk time.** A manifest carries
  sizes so that offering a directory does not read every byte of it; the
  hashes each `FileTransferStart` carries are computed once the far side has
  said yes, in manifest order, and a file that cannot be read cancels the
  whole group.
- **Files are announced and sent `MAX_CONCURRENT_FILE_TRANSFERS` at a time.**
  The receiver refuses a fourth concurrent start, so `start_pending_sends`
  now honours the same bound the receiver enforces — it never showed with a
  clipboard copy of three files, and it is the first thing a directory would
  have hit.

A directory reaches the wire from the file manager of ADR 0076, in both
directions: an upload names the destination, and a download of a directory is
answered with a manifest instead of an offer. The clipboard path is unchanged
and still offers files only.

### Resume, and what is honestly not implemented

Two different things were called "resume", and only one of them is here.

**A file connection that drops while the session lives** is now resumed. The
send job survives in `file_resumable`, a stream that ends early is picked up
from the offset the receiver acked, `FILE_RESUME_ATTEMPTS` bounds the retries,
and both grants are re-read before it starts again. This is what makes
`SendJob::from` mean something for the first time.

**A session that drops and is re-established** is not. §10's window,
`ReconnectWindow` and `SessionManager::on_reconnect` all exist in
`lumepeer-core` and `lumepeer-net`, and **the desktop actor never calls
`on_reconnect`**: a closed control connection ends the session, and the next
connection from the same device queues a fresh consent request — which
`network.rs`'s own test asserts, deliberately. So a transfer cannot survive a
control-connection drop today, because the session does not either. Making it
survive is implementing §10's session resume, which is a decision about
consent and not about files, and it is not made here.

### Staging, and what happens when the app is killed

Staging stays beside the destination. With a 64 GiB ceiling the alternative —
staging under the app's own data directory — turns every export on a different
volume into a second full pass over the file, which is the difference between
a transfer that finishes and one that copies itself again.

The price is that a process killed mid-transfer leaves a `.lumepeer-<id>.part`
in somebody's own folder, because every path that removes one — cancel, abort,
hash mismatch, session end — runs inside a process that is no longer there.
So: **a partial file is deleted, at the latest the next time anything is
received into that directory.** `file_transfer::sweep_stale` runs before each
staging file is created and removes only names matching `.lumepeer-<id>.part`
exactly, only files, and only ones modified before this process started — so a
transfer running right now, in that directory or any other, is never touched.

The hash is still verified over the whole file at the end, and a partially
received file still never leaves staging.

## Consequences

- A 64 GiB file can be offered, and a 5 GiB one is ordinary. The transfer is
  as fast as the link; nothing here changes the chunking.
- A peer below minor 14 sees no behaviour change at all: it is never offered
  more than it accepts, and it is never sent a manifest.
- A directory of more than 200 files cannot be sent as a directory. That is a
  real limit and a visible one — the offer is refused rather than trimmed —
  and raising it means either a bigger control frame or a manifest that
  spans frames, neither of which this needed.
- A tree with a file that changes size between the walk and the send is
  cancelled rather than delivered wrong.
- Free space is checked once, before accepting. A volume that fills up from
  somewhere else during a long transfer still fails the way it always did.

## Alternatives considered

- **Archive the directory** (`zip`, `tar`). Rejected in the batch's own terms
  and for the reasons above: resume, inspectability, and a dependency.
- **Remove the size ceiling entirely** and let free space be the only bound.
  §9.1 wants a static bound on every number an untrusted peer sends, and a
  peer claiming `u64::MAX` would otherwise reach the free-space check as a
  legitimate offer.
- **Move staging into the app data directory** so a crash leaves nothing in a
  user's folder. Costs a full extra copy on export across volumes, which at
  this ceiling is the whole point of the change.
- **Sweep staging at startup instead of at use.** Would need a persisted list
  of every directory ever staged into — a new on-disk store, for a cleanup
  that the next transfer into that directory does for free.
- **Implement §10 session resume here** so a transfer survives a reconnect.
  It is a consent decision (whether a re-established session inherits its
  grants) and belongs with gap-tasks 24, not inside a file-transfer batch.
