# ADR 0075 — Listing a directory is its own grant

Status: accepted
Date: 2026-09-09

Extends [ADR 0032](0032-file-transfer-start-and-the-lazy-file-connection.md),
which built the file transfer engine and its lazily-opened `rd/file/1`
connection, and [ADR 0054](0054-full-control-carries-every-independent-grant.md),
which settled that `Role::FullControl` carries every independent grant.
Follows [ADR 0048](0048-a-fifth-independent-grant-for-the-hosts-own-screen.md)
in shape: a capability that is not implied by anything already granted gets a
flag of its own.

## Context

Today a file can be **offered**, and that is all. `FileOffer`, `FileAccept`,
`FileTransferStart`, the chunk format, the staging directory and the BLAKE3
check are all there; what is missing is any way for a guest to find out what
is on the host to ask for. There is no listing message in the protocol in any
form, so the file manager of backlog item 19 has nothing to be built on.

The tempting shortcut is to serve a listing under `file_transfer`, on the
grounds that a session which can already receive files may as well see which
ones there are. That reads the grant backwards. `file_transfer` covers files
**both sides have already named to each other**: the host chose the file, or
approved the offer, in every case. A directory listing is the opposite — the
guest names a place and the host enumerates it, disclosing project names,
client names, `Downloads`, the shape of somebody's work, and by repetition the
whole tree, none of which the host picked.

## Decision

**A new independent grant, `file_browse`**, and two new messages behind it.

- `Grants::file_browse` / `IndependentGrant::FileBrowse`, carried by
  `Role::FullControl` alone (ADR 0054) and by neither lesser role. It is not
  implied by `view`, by `input` or by `file_transfer`, and a session may hold
  `file_transfer` without it — which is the implication this grant exists to
  refuse, and is what `consent.rs`'s own test asserts.
- `PROTOCOL_MINOR` 12 appends `DirListRequest { path }` and
  `DirListResponse { entries, truncated, refused }`, behind the
  `FEATURE_FILE_BROWSE` string. One request, one directory: there is no
  recursive form, because a recursive walk is an unbounded amount of the
  host's disk read on a peer's say-so.
- The listing rides the **control channel**. `rd/file/1` is opened lazily and
  only after `FileAccept(true)` precisely so that revoking a grant is never
  delayed by a transfer (ADR 0032); opening it to read a directory would undo
  that for a message that transfers nothing.
- A `DirEntry` carries a name, a size, whether it is a directory and a
  modification time. Not attributes, not the owner, not permissions, not a
  link target: those describe the host's machine and its accounts rather than
  the file a guest is deciding whether to ask for (§15).

### Refusals are said out loud

An empty list means an empty directory, and nothing else. Every other outcome
carries a `DirListRefusal`:

- `NotGranted` — no `file_browse`, whether it was never held or was withdrawn
  a moment ago;
- `BadPath` — the path was refused before it reached the filesystem;
- `Unreadable` — the operating system refused it, or it is not a directory.

`Unreadable` is deliberately **one** variant rather than "no such directory"
and "permission denied" told apart. That pair is an oracle for what exists on
the host, which is the one thing a listing must not answer about places the
guest may not read.

The grant is re-read **when the request arrives**, not when the session
started — the same discipline every input event and every `DisplaySetMode`
already follows (§2.3). A host that changes its mind is obeyed by the next
request, and that is a test rather than a claim.

### The path is a string an attacker chose

`safe_file_name` in `lumepeer_net::file_transfer` already treats an offered
file name as hostile. `lumepeer_core::remote_path` does the same for a whole
path, and it is its own string parser rather than `std::path`:

The host and the guest need not be the same operating system, so a Linux host
has to reject `..\..\windows` and a Windows host has to reject `../../etc`,
while `std::path::Component` only knows the separators of the platform it was
compiled for. Everything in that module treats `/` and `\` as separators and
checks the Windows device names on every platform, which is the only reading
that is safe on all of them. Refused, never rewritten: sanitizing a hostile
path produces a listing of a directory neither side named.

Symbolic links are not followed. A path that *is* a link is refused, and a
link inside a listing is reported as the ordinary entry it is rather than as
the directory it points at — "where does this link go" is a question about the
host's own layout, and answering it with a listing would let a guest walk out
of the place it named.

Every request that reaches the grant check is audited, refused or not: what
§15 wants recorded is that this peer asked to read this machine's disk. The
peer travels as a salted hash and the path does not travel at all.

## Consequences

- `PROTOCOL_MINOR` is 12 and `tests/interop/golden_vectors.txt` has five new
  vectors appended; every earlier line is byte-for-byte what it was.
- `MAX_DIR_ENTRIES_PER_RESPONSE` is 200, and a compile-time assertion states
  why that number and not another: 200 worst-case entries still fit
  `MAX_CONTROL_FRAME_BYTES`. A directory with more is answered with
  `truncated: true` rather than quietly cut short.
- The guest side of the core is here too — `request_dir_list` and
  `dir_listing` on the actor handle, and the listing kept on the view exactly
  as `DisplayModesList` is. There is no UI: the file manager window is
  gap-tasks `14`, and `commands.rs` was deliberately not touched, so nothing
  in the app reaches this yet and no host can turn `file_browse` on from a
  switch. A `FullControl` session has it; nothing else can get it until that
  batch adds the toggle.
- The directory is read on the actor thread. That is the choice this actor
  already makes for filesystem work, and a listing is bounded — but a
  directory on a disconnected network share is where it costs, and it would
  stall every other session on this host while it timed out.

## Still open

- **No entry point.** A guest can list a directory it can name, and nothing
  tells it what to name first: there is no message for "which drives does this
  host have" or "where is the home directory". `DirListRequest` demands an
  absolute path, so gap-tasks `14` needs a starting point that does not exist
  yet — either a roots message, or a convention.
- **Reading off the actor thread**, as above.
- **No paging.** `truncated` says a directory had more than 200 entries and
  offers no way to see the rest. A cursor is the obvious fix and belongs with
  the UI that would need it.
