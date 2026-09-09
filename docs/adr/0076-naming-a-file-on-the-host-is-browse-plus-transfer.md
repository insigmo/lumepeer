# ADR 0076 — Naming a file on the host is browse plus transfer

Status: accepted
Date: 2026-09-10

Extends [ADR 0075](0075-listing-a-directory-is-its-own-grant.md), which added
the `file_browse` grant and the two listing messages, and
[ADR 0032](0032-file-transfer-start-and-the-lazy-file-connection.md), whose
transfer engine this reuses whole. Follows
[ADR 0061](0061-full-control-carries-the-uac-click.md) in one respect:
where a confirmation would only re-ask a question `Role::FullControl` has
already answered, it is not asked.

## Context

After ADR 0075 a guest can see what is on the host and can do nothing with
it. The transfer engine of §9.2 moves a file only when the **sender** picks
it — a copy on the host's own clipboard, or an offer the host made — so the
one gesture a file manager is for, "that file there, bring it here", has no
message to travel on. The same hole exists in the other direction: an upload
lands wherever the *receiving* user picks in a dialog, which is right for a
file that arrives unasked and wrong for a file dropped into the directory the
guest is looking at.

Two questions had to be answered before any of it could be built.

**Which grant covers it.** `file_browse` is the right to *see*, and it is not
the right to move bytes; `file_transfer` is the right to move bytes both
sides have named, and after ADR 0075 it is explicitly not the right to see.
Naming a file on someone else's disk and taking it needs both halves, and
neither half implies the other.

**Whether the host confirms each transfer.** An upload writes into the host's
filesystem, which is more than any earlier grant did on its own.

## Decision

**Three messages, one feature string, no new grant, and no new dialog.**

- `PROTOCOL_MINOR` 13 appends `FileFetchRequest { path }`,
  `FileFetchRefused { reason }` and
  `FilePutOffer { dir, name, size, hash }`, all gated on a new
  `FEATURE_FILE_MANAGE` string, all guest to host (the refusal is the host's
  answer to one). Appended at the tail, with five golden vectors and every
  earlier line unchanged.
- **The host requires `file_browse` *and* `file_transfer`** for all three,
  re-read at the moment each message arrives rather than trusted from when
  the session started — the discipline ADR 0075 applies to a listing and
  §2.3 applies to every input event. Either grant withdrawn mid-copy refuses
  the next file.
- **A fetch is answered by the ordinary `FileOffer`.** No second transfer
  path exists: same `rd/file/1` connection opened lazily by ADR 0032, same
  chunk format, same length check before allocation, same BLAKE3 verified
  before anything leaves staging. `FileFetchRefused` exists only because a
  refusal has no offer to travel on.
- **The guest's own request is the acceptance.** An offer that answers a
  fetch is not queued as a decision to make: the click that asked for it was
  the decision, and it lands in the directory the local pane was already
  showing, so no picker runs. Matched on the front of the pending-fetch queue
  by basename — a fetch is answered by exactly one offer or one refusal.
- **A put is an offer with a destination**, answered by the ordinary
  `FileAccept`. Both halves of the destination are parsed as untrusted input:
  the directory by `remote_path::safe_browse_path`, the name by
  `safe_file_name`, and the file lands *beside* one of the same name rather
  than over it (`unique_destination`), because an upload that overwrites is
  also a delete, and deleting on the host is not something any grant here
  covers.
- **No host-side dialog per file.** `Role::FullControl` plus both grants is a
  host that has already handed over the keyboard on an elevated window
  (ADR 0061); a confirmation for each file of a copy the operator is watching
  happen is a prompt that gets clicked through rather than read. Every fetch
  and every put is audited instead, peer as a salted hash, path never
  recorded (§15).
- **`FileFetchRefusal` is its own enum**, not a reuse of `DirListRefusal`.
  The first three variants say the same three things, and the fourth —
  `TooMany` — has no meaning for a listing; adding it to the frozen enum
  would put a variant on the wire that every peer built before minor 13 reads
  as malformed. `TooMany` bounds outstanding fetches per peer by
  `MAX_PENDING_FILE_OFFERS`, counting hash passes in flight as well as offers
  already made, because the hash pass is where the cost is.

### The window

Five IPC commands, registered in `invoke_handler`, `build.rs`'s `COMMANDS`
and `capabilities/view.json` (and its pilot copy): `local_dir_list`,
`remote_dir_list`, `remote_dir_status`, `remote_download`, `remote_upload`.
None of them authorizes anything.

Two of the five are more than the batch named, and both are forced by the
shape of what already exists. `remote_dir_status` is separate from
`remote_dir_list` because the actor's listing request is fire and forget, as
every guest-to-host control request is — the answer is polled, not returned.
`local_dir_list` exists because the local pane needs a listing and a view
window has no filesystem rights of its own; it goes through the same path
parser a path from the wire goes through, and through the same
`network::read_directory` the host uses to answer a listing, so the two panes
cannot come to disagree about what an entry is.

The remote pane finds its starting directory by probing `/` and then `C:\`.
No message asks a host where it keeps its files, and none is added for this:
which absolute path exists is exactly what a listing request already answers.

Where the grant is missing, the panel is **not drawn and its toolbar button
is absent** — including when the grant is withdrawn mid-session, which the
poll notices within a second. A refusal a person can do nothing about from
inside the panel is not an error message; it is a panel that should not be
there.

## Consequences

- A guest with full control can read and write the host's filesystem through
  a window, at the speed of the existing transfer engine. That is what
  backlog item 19 asks for, and it is bounded by two grants either of which
  ends it.
- The host has no per-file veto on an upload. The audit log has every one of
  them, and both grants are revocable mid-copy, but a host that wants to
  approve files one at a time has to withhold a grant instead. Written down
  here because it is a real reduction in what a host is asked, and the
  alternative — a dialog per file — was rejected as one that gets clicked
  through.
- A put can fill the host's disk. Nothing here bounds total bytes; the
  per-offer ceiling is `FILE_OFFER_MAX_BYTES` and the concurrency ceiling is
  `MAX_CONCURRENT_FILE_TRANSFERS`, both unchanged. Free space before a
  transfer starts is gap-tasks 15's problem, not this one's.
- Renaming, deleting and creating directories on the host stay absent. They
  are a different kind of write, and they would need their own grant and
  their own decision.

## Alternatives considered

- **Serve fetch and put under `file_transfer` alone.** Refused for the reason
  ADR 0075 refused serving a listing under it: the guest names the file, and
  a grant about files both sides already named cannot cover that.
- **A third grant, `file_manage`.** A grant nobody could hold without the
  other two is a switch with one meaningful position. The intersection of the
  two existing grants says the same thing and leaves the consent screen the
  length it is.
- **Let the fetched offer land in the ordinary offer list.** Simpler in the
  actor by about sixty lines, and it asks the person who just pressed
  Download to answer a dialog about the file they asked for, in a panel that
  lives in a different window.
- **Ask the host where its home directory is.** A new message, a new feature
  string and a new disclosure, to save two probes that the listing request
  already performs.
