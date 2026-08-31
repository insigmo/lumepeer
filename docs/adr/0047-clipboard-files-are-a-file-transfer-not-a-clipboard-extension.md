# ADR 0047 — Clipboard files are a file transfer, not a clipboard extension

Status: accepted
Date: 2026-09-01
Extends: §9.2 (file transfer), §2.2/§8.2 (independent grants), §15 (what is
never recorded)
Builds on: ADR 0032 (`FileTransferStart` and the lazy `rd/file/1`
connection), ADR 0029 (independent grants are issuable), ADR 0030 (the
clipboard crosses the wire and the OS)

## Context

`docs/bugs/14-clipboard-files.md` ("если файл скопировали на 1 хосте, можно
вставить на другом хосте") asks for one more way to get a file from one
machine's clipboard onto another's: copy a file in a file manager, paste it
into the remote session. The file-transfer engine this needs already exists
in full — `FileOffer`/`FileAccept`/`FileTransferStart`, its own lazily-dialed
`rd/file/1` connection, `FILE_OFFER_MAX_BYTES`, BLAKE3 verification before
export, authorization through the `file_transfer` grant (ADR 0032). What was
missing was reading a *list of file paths* out of the OS clipboard at all —
`clipboard_os.rs`'s `OsClipboard` trait handled text only (§9.2 v1), and none
of `arboard`'s formats cover `CF_HDROP`, `text/uri-list` or
`NSPasteboardTypeFileURL` either.

`docs/bugs/DECISIONS.md` D5 point 2 settles the product question already:
files through the clipboard are a separate feature from text, always routed
through the existing file-transfer engine, and receiving a file stays a human
decision. What this ADR records is the three implementation choices that
decision still left open, and how the feature fits against ADR 0032 and
against `docs/bugs/10-clipboard-auto.md` (batch 6, automatic text sync).

**A note on that last point, because it changes what "builds on" means
here.** At the time this ADR was written, batch 6 had not landed on the
branch this work is based on: `clipboard_os.rs` carries no guest-side
clipboard watching, and `toolbar.ts` still has the manual clipboard button
ADR 0030 described. This feature does not depend on batch 6 having landed —
see the last section — but a reader comparing this ADR against a `master`
where batch 6 *has* since landed should expect `clipboard_os.rs`'s header
comment and `toolbar.ts` to look further along than they do in the commits
this ADR accompanies.

## Decision

### This is `file_transfer`, not a third clipboard grant

A file offered through the clipboard runs under the `file_transfer` grant
(§8.2) and nowhere else. It is checked exactly where an ordinary offer
already is: `NetworkActor::may_transfer_files`, unchanged by this feature.
Neither `clipboard_read` nor `clipboard_write` is consulted, and having both
of them on with `file_transfer` off refuses the offer just as completely as
having none of the three — `tests/network::tests::
clipboard_files_need_file_transfer_not_the_clipboard_grants` proves this over
two real actors, and `clipboard::permits_files`'s own unit test proves the
grant arithmetic in isolation.

The alternative — a new independent grant, or folding this into
`clipboard_read`/`clipboard_write` — was rejected for the reason D5 states
directly: this is a file transfer with a different entry point, not a
clipboard capability. `clipboard_read`/`clipboard_write` are about whether a
peer may see or change *this machine's clipboard as text*; nothing about
either grant says anything about receiving arbitrary files on disk, which is
squarely what `file_transfer` already means and already bounds
(`FILE_OFFER_MAX_BYTES`, `MAX_PENDING_FILE_OFFERS`, BLAKE3 verification). A
host that has turned `file_transfer` off has said "no files reach my disk
through this session", and a clipboard paste is not an exception to that
sentence merely because the file's name arrived over a different message
type. Reusing the grant also means every limit and every audit tag the
engine already has apply here for free, rather than a parallel set of limits
this feature would otherwise have had to invent and keep in step by hand.

### The announcement carries names and sizes, never paths

`MessageKind::ClipboardFileOffer { files: Vec<ClipboardFileEntry> }` is new
(`PROTOCOL_MINOR` 8, appended after `StreamScaleRequest`, behind
`FEATURE_CLIPBOARD_FILES` exactly as `docs/tasks/03-file-transfer.md` task 1
requires for a new file-transfer message: a new `FEATURE_*` string, sent only
to a peer that advertised it, golden vectors appended, a negative test in
`protocol_negative.rs`). `ClipboardFileEntry` carries a `name` and a `size`
and nothing else — no path, and no hash either.

No path, because a full path is information about the *sending* machine that
the receiver has no legitimate use for: a username embedded in
`C:\Users\alice\Desktop\...`, an internal project layout, a mounted drive
letter. §15 already keeps this off the wire for an ordinary `FileOffer`
(`OutgoingOffer` keeps the path locally and only ever sends the basename);
this message keeps the same property for a list of them.

No hash, unlike `FileOffer`. Hashing is a full disk pass (ADR 0027), and a
clipboard announcement happens before anyone has agreed to receive anything —
hashing every file a user happens to have copied, on the chance one of them
gets accepted, would be exactly the kind of work ADR 0027 exists to keep off
paths that do not need it yet. The hash is computed once, in the same
`prepare_offer` an ordinary offer already uses, only for the specific entry
the peer accepts (`on_clipboard_file_accept_inbound`'s spawned task). What
this trades away is the bait-and-switch check `on_file_transfer_start`
performs for an ordinary offer — comparing the start's hash against the one
shown at offer time. A clipboard-accepted entry has no such hash to compare
(`AcceptedOffer.hash` is `Option<[u8; 32]>`, `None` for this path), so that
one check is skipped for exactly this case. Nothing about integrity is lost
by skipping it: `ReceiveTracker::finish` still verifies the real bytes
against whatever hash `FileTransferStart` carries, which is the check that
actually protects the file on disk from transit corruption. What the skipped
check would have added is protection against a sender that shows one file's
metadata and starts a different one — a narrower guarantee this feature
trades for not hashing files nobody has asked for.

### Accepting a clipboard entry is the one human decision, and it is never skipped

The receiving user's acceptance of one `ClipboardFileOffer` entry is
answered with `MessageKind::ClipboardFileAccept(bool)` — shaped like
`FileAccept` for the same reason: entries are answered one at a time, oldest
first, through the same FIFO queue an ordinary offer already uses
(`file_offers_in` now holds an `IncomingOffer` enum with `Direct` and
`Clipboard` variants side by side, so the panel's "accept the oldest offer"
button works identically regardless of which kind is at the front). Once
accepted, the transfer starts through the *existing* engine —
`FileTransferStart`, chunks over `rd/file/1`, BLAKE3 verification before
export — without a second `FileOffer`/`FileAccept` round trip in front of
it. That is a deliberate reading of "receiving stays a human decision": the
decision is the accept of the clipboard entry itself, made with the name and
the size already in view (`file-transfers.ts` tags the row "from clipboard"),
and asking again once the human has already agreed would be asking the same
question twice, not adding a second decision.

Nothing here writes a file to disk without that acceptance, under any
setting: `on_clipboard_file_offer_inbound` refuses outright without
`file_transfer`, and `on_clipboard_offer_accept` re-checks the grant again
before creating the receive directory or acknowledging the peer.

### The destination is fixed, not chosen, and it is cleaned up

An ordinary accepted offer lets the receiving user pick a directory (the OS
picker, run from Rust — ADR 0032). A clipboard-accepted entry does not: it
always lands in `crate::config::clipboard_files_dir()/<peer-tag>/`, a
per-peer subdirectory of this application's own data directory. The `file_
accept` IPC command skips the directory-picker dialog whenever the offer
being answered is tagged `from_clipboard` — a hint the webview supplies for
UX only; the actor decides the real destination itself regardless of what
that hint says, and a mismatched hint costs at most an unnecessary dialog or
a clean refusal, never a widened grant or a redirected write.

Fixing the destination is what makes "paste" a verb that actually works:
once the transfer completes and the hash has verified, the receiving
machine's own OS clipboard is set to the file's new path
(`ClipboardWorker::write_files`, the write-side counterpart this ADR adds
next to the read side), so a `Ctrl+V` in the receiving user's file manager
produces the file. The per-peer subdirectory is removed the moment that
peer's session ends (`abandon_file_transfers`), and the whole `clipboard-
files` directory is swept once at actor startup, in case a previous run
ended without going through that path at all — a leftover file from a past
session is a leak, not a convenience.

### Reading the OS clipboard's file list needed a library choice

`arboard` — the crate already in the tree for text — covers none of the
three platform formats. `docs/bugs/14-clipboard-files.md` allows either
writing the platform code directly or choosing another library, provided any
new dependency clears `deny.toml`.

The choice made per platform, each picked to add no new dependency *version*
to `Cargo.lock`:

- **Windows**: `clipboard-win`, at the exact version `arboard`'s own Windows
  text backend already resolves to (5.4.1). Its `formats::FileList` wraps
  `CF_HDROP`/`DragQueryFileW` with no `unsafe` on this crate's side of the
  boundary — worth choosing over raw FFI given the workspace's
  `unsafe_code = "deny"` lint, which raw `windows`/`windows-sys` calls would
  have had to carry an `#[allow]` past.
- **Linux (X11, and Wayland through XWayland)**: `x11-clipboard`, a small
  crate over `x11rb` 0.13, the same major version `arboard`'s own Linux
  backend already depends on. It supplies the ICCCM selection-owner and
  -requestor machinery (`TARGETS`, `INCR` for anything larger than one
  property, the background thread that keeps serving a stored value) that
  hand-rolling would otherwise have had to reimplement; this feature only
  supplies the `text/uri-list` target and the URI encoding on top of it.
  This inherits the same gap the existing text path already has: no native
  Wayland clipboard protocol, so a compositor with no XWayland compatibility
  serves neither. Not a new limitation — `arboard`'s Linux backend is X11-
  only today too.
- **macOS**: `objc2-app-kit`/`objc2-foundation`, the same crates and pinned
  versions (`0.3.2`) both `arboard`'s macOS text backend and
  `lumepeer-media`'s `capture-screencapturekit` already depend on.
  `NSPasteboard`/`NSPasteboardItem`'s relevant methods are safe functions in
  this binding generation; the one `unsafe` block this feature adds reads an
  `extern "C"` static (`NSPasteboardTypeFileURL`) that objc2's generated
  code has no safe accessor for, justified the same way `crates/media/src/
  sas.rs`'s `SendSAS` call already is (ADR 0012's standard: a raw FFI entry
  point with no safe binding).

A general-purpose cross-platform clipboard crate (one was considered:
`clipboard-files`, read-only, pulling `gtk` 0.18 on Linux and the legacy
`objc` crate rather than `objc2` on macOS) was rejected specifically because
it would have introduced a second, differently-versioned dependency graph
next to the one `arboard`/`tauri`/`lumepeer-media` already established, for a
capability (reading, not writing) narrower than what this feature needs.

### Limits are checked before this side allocates, as far as each platform lets it

`CLIPBOARD_FILE_LIST_MAX_ENTRIES` (equal to `MAX_PENDING_FILE_OFFERS` — more
entries than the transfer engine could ever queue would only be declined
further down the same pipeline) and `CLIPBOARD_FILE_PATH_MAX_BYTES` (4096,
covering `PATH_MAX` on Linux/macOS and generous for Windows) are both new
constants in `crates/core::constants`, and both are checked before this
process allocates a `PathBuf` per entry: a raw byte-length ceiling on the
whole `CF_HDROP`/`text/uri-list` block before either platform arm asks its
library to walk it, and an entry-count ceiling on macOS's `NSPasteboardItem`
array before iterating it. What none of these can do is reach *inside*
`clipboard-win`/`x11-clipboard`/`objc2-app-kit`'s own internals and bound
their allocations before the fact — the same is already true of `arboard`'s
text path, which nobody audits for this either. A clipboard file list is
untrusted input from another local application, which is a real but
substantially narrower threat model than an unauthenticated remote peer's
wire message; the wire side of this feature (`ClipboardFileOffer`'s
`check_limits`) gets the stronger guarantee, because postcard's decoder is
exactly the boundary §9.1 was written to bound precisely.

## Consequences

- A file copied on one machine's clipboard reaches the other side's disk
  only after that side's user explicitly accepts the named, sized entry —
  never automatically, under no configuration.
- `apps/desktop/src-tauri/Cargo.toml` gains three small platform-gated
  dependency blocks (Windows, Linux, macOS) plus `percent-encoding`
  unconditionally; none of them introduce a new resolved version anywhere in
  `Cargo.lock` that was not already there via `arboard`, `tauri`/`tao`, or
  `lumepeer-media`.
- `PROTOCOL_MINOR` is 8. `ClipboardFileOffer` and `ClipboardFileAccept` sit
  at discriminants 39 and 40, after `StreamScaleRequest`; every earlier
  discriminant is unmoved and the frozen golden vectors still pass, with two
  new ones appended for minor 8.
- `IncomingOffer::Clipboard`/`AcceptedOffer.from_clipboard` are the only
  places the receiving side's code needs to know an offer came from a
  clipboard rather than a picker; `file-transfers.ts` reads that flag to
  show the "from clipboard" tag and to skip the directory-picker dialog on
  accept.
- Text sync is untouched: `clipboard_read`/`clipboard_write` still gate only
  `ClipboardSync`, and `crates/core::clipboard`'s module comment now says so
  explicitly, pointing at `permits_files` for the file path instead.
- This ADR does not depend on `docs/bugs/10-clipboard-auto.md` (batch 6,
  automatic guest-to-host text sync) having landed, and at the time it was
  written that batch had not — see the note in Context. The reason it does
  not need to: the guest-side "may I do this" question for files is already
  answered by `may_transfer_files`'s existing `self.views.contains_key(peer)`
  check (an open view, not a grant the guest lacks), the same mechanism ADR
  0032 already established for an ordinary offer. Batch 6's contribution
  would have been a continuously-polled guest-side clipboard *watch*, which
  this feature deliberately does not need: reading the clipboard for files
  is a one-shot, human-triggered action (`file_offer_clipboard`), not a
  background poll, so it was never coupled to that infrastructure.

## Verification

- `crates/core::protocol`: `ClipboardFileOffer`/`ClipboardFileAccept`
  roundtrip; a list past `CLIPBOARD_FILE_LIST_MAX_ENTRIES`, a name past
  `FILE_NAME_MAX_BYTES` and a size past `FILE_OFFER_MAX_BYTES` are each
  malformed; the exact bound on every axis is still ordinary traffic; the
  new discriminants sit at 39 and 40.
- `tests/integration/tests/protocol_negative.rs`: the same three bounds
  refused on a live connection, not merely on a hand-built envelope.
- `tests/interop/golden_vectors.txt`: two vectors frozen for minor 8.
- `crates/core::clipboard`: `permits_files` requires `file_transfer` and
  ignores both clipboard grants in either direction.
- `apps/desktop/src-tauri/src/network.rs`:
  `clipboard_files_need_file_transfer_not_the_clipboard_grants` — two real
  actors, both clipboard grants on and `file_transfer` off refuses; the
  identical offer succeeds once `file_transfer` is what is actually on.
- `apps/desktop/src-tauri/src/clipboard_os.rs`: the `OsClipboard` trait's
  `read_file_paths`/`write_file_paths` roundtrip through the same
  `TestClipboard` seam the text path already uses; `NoClipboard` reports
  unavailable for both rather than a silent empty success.
- `apps/desktop/src/file-transfers.test.ts`: the clipboard-offer row is
  tagged and answered with `from_clipboard`; the send-from-clipboard button
  supplies only the peer label, never a path.

## What this session could not verify

The Windows platform arm (`clipboard-win`) compiled and its unit tests ran on
the machine this work was done on, since it targets `x86_64-pc-windows-msvc`
directly. The macOS (`objc2-app-kit`/`objc2-foundation`) and Linux
(`x11-clipboard`) arms compiled only in the sense that `#[cfg]` excluded them
from every build this session ran — there was no macOS or Linux display
available to build or exercise them against. Both are written against
documented APIs and mirror this codebase's own established macOS/Linux
patterns (`crates/media`'s `objc2` usage, `arboard`'s own backend shape), but
neither has been compiled, let alone run, on its target platform.
