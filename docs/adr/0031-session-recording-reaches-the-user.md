# ADR 0031 — Session recording reaches the user, and exports without a Matroska muxer

Status: accepted
Date: 2026-08-27
Extends: §17 (session recording), §9.2 (`RecordRequest`/`RecordAck`), §2.2
(no hidden capture), §2.3 (the webview authorizes nothing), §15 (what is never
recorded)
Builds on: ADR 0023 (the `LMREC` container, "MKV export as a later, separate
step"), ADR 0029 (the four independent grants became issuable), ADR 0030 (the
shape of a feature that was end-to-end except for both ends)

## Context

§17 recording was built and unreachable, in the same shape ADR 0030 described
for the clipboard.

- `crates/media/src/record.rs` wrote and read the container.
- `apps/desktop/src-tauri/src/recorder.rs` ran the writer thread.
- `Actor::on_record_toggle` checked the grant and fed the media loops.
- The `recording_toggle` IPC command existed.

And: **no UI called it**, `grep -rn recording-toggle capabilities` was empty
so a call would have been refused by Tauri's ACL anyway,
`MessageKind::RecordRequest`/`RecordAck` were handled nowhere on either side,
the destination path arrived from the webview as a free string, and the
resulting `.lmrc` file opened in no player.

The last two are the ones that were not merely missing but wrong. A path
chosen by the untrusted view layer decides where this process writes a file,
which is §2.3's line exactly. And a recording nothing can play is not a
recording; it is a private format with a promise attached.

## Decision

### The destination is decided in Rust and only reported outwards

`recording_toggle` takes `{peer, on: bool}`. The path comes from
`config::recordings_dir()` (the per-user data directory plus `recordings`),
the file name from the clock and the peer's pseudonymized label, and
`network::recording_path` is the only place either is assembled. Starting a
recording answers with the path so the operator can find the file; stopping
answers `None`.

The direction of that value is the whole point: a path travels *out* of the
core to be shown, never *in* from the webview to be obeyed. If the host user
should be able to choose a directory, that is a native dialog on the Rust
side, not a string from a view layer that a compromised page can write.

### Both sides are told, on every frame and over the wire

§2.2's "no hidden capture" is a claim about what the two people in the session
can see without going looking, so:

- The host gets a banner in the main window for as long as any session is
  being recorded, plus a per-session badge, neither dismissable.
- The guest gets a badge over the picture, outside the toolbar. The toolbar
  collapses and can be dragged away; an indicator someone can put away is not
  an indicator.

The guest's badge is driven by `MessageKind::RecordAck`, which the host now
sends **unsolicited** when a recording starts or stops, not only in answer to
a request. The guest cannot know what the far side writes to disk, so what it
displays is the host's own statement and never an inference. The flag rides
the `view_next_frame` response on every poll, in the byte that used to carry
`input` alone and is now a flags byte (`VIEW_FLAG_INPUT`,
`VIEW_FLAG_RECORDING`) — for the same reason `input` rides along: a state that
can change mid-session must not be something the window was told once.

No new `MessageKind`, no `PROTOCOL_MINOR` bump, no `FEATURE_*` string.
`RecordRequest` and `RecordAck` have held their discriminants since minor 1
and are in the frozen golden vectors of §17.2, so every build that speaks this
protocol already decodes them.

### A guest may ask; asking decides nothing

`RecordRequest` arrives, is refused outright unless the session is `Active`,
is charged against the peer's `ConsentRateLimiter` budget — the *same*
limiter type the consent path uses, because this also puts a dialog in front
of a person — and is then parked in `Actor::record_requests` for the host user.
It is surfaced through the session status the UI already polls, so no new
notification channel exists to keep in step.

Answering is the host pressing a button:

- **Start recording** turns the `recording` grant on through
  `SessionManager::set_grant` (ADR 0029) if it is not on already, then calls
  `recording_toggle`. The order is not cosmetic: permission first, act second,
  and the core decides both.
- **Not now** calls `recording_toggle{on:false}`, which clears the request and
  sends `RecordAck(false)`.

An automatic yes was never an option: `recording` is a separate grant in §8.2
precisely so that the decision belongs to the person at the host. A refusal is
an ordinary answer the guest is told about, not an error and not a silence.

### Export: two elementary streams, because there is no muxer to use

ADR 0023 deferred "MKV export as a later, separate step". That step lands
here, without the MKV.

No Matroska muxer in the Rust ecosystem passes this workspace's supply-chain
policy: the maintained crates in that space are demuxers (`matroska`,
`matroska-demuxer`), and muxing would mean either an unmaintained crate or a
C library binding — the second `cargo deny` exception of its kind after
`audiopus_sys`, for a feature nobody is blocked on.

So `crates/media/src/export.rs` writes what the container already holds, each
in the plainest wrapper that makes it playable **with no new dependency at
all**:

- video → `<name>.h264`, the Annex-B chunks concatenated in capture order.
  They already carry their own start codes, so the elementary stream is the
  concatenation and no framing is invented.
- audio → `<name>.opus`, Opus in Ogg (RFC 3533 pages, RFC 7845 headers),
  about 150 lines of page writer, CRC and TOC-driven granule positions.

Raw Opus packets in a file would have been the cheaper "honest minimum" the
task allowed, and it was declined: no player opens that, and an export nobody
can play is the same promise-with-nothing-behind-it this ADR started with.
`ffprobe` reads the result as `Audio: opus, 48000 Hz, stereo` with the correct
duration.

Both tracks stream: the source is read one record at a time through the new
`record::RecordReader`, and each payload is written before the next is read,
so an hour-long session costs one record of memory. `read_recording` is now a
loop over that same reader — one parser, not two. Event records are dropped:
they are the session's action log, and a player has nowhere to put them.

## Consequences

- A recording still requires the `recording` grant, and still dies with the
  session (`stop_media` flushes it). Nothing here widens what ADR 0029 made
  reachable; it only makes the switch pressable.
- `RECORD_FORMAT_VERSION` is untouched. The container did not change — only
  what reads it.
- Exporting produces two files rather than one, and a player has to be pointed
  at each. If a muxer ever clears the policy, the exporter gains an MKV target
  and these two stay: they cost nothing and they are what a scripting user
  wants anyway.
- The `.h264` stream carries no frame rate, and the two files carry no shared
  clock, so lip-sync across them is approximate. The `.lmrc` keeps the
  per-record timestamps that would fix this the day there is a container to
  put them in.
- `SessionRecorder` now counts the records a full queue drops
  (`SessionRecorder::dropped`) and says so on stop. Dropping was always the
  policy — the session's picture matters more than its recording — but a
  recording with holes has to be able to say it has them (§24.5).
- The recording path is reported to the UI and deliberately **not** written to
  the audit log: §15 keeps paths out of it, so `AuditEvent::RecordingToggled`
  carries the state change and nothing else.

## Verification

- `crates/media/src/record.rs`: round-trip of kinds, order and timestamps; a
  truncated tail yields the valid prefix; corruption in the middle is an error
  rather than a short read.
- `crates/media/src/export.rs`: video concatenates into one Annex-B stream;
  audio becomes Ogg pages whose checksums a demuxer recomputes and whose final
  granule position is the true duration; a truncated recording exports its
  valid prefix; an unreadable Opus TOC stops the export instead of skewing
  every later timestamp; a video-only recording leaves no empty `.opus`
  behind.
- `recorder.rs`: a stalled sink fills the queue, and the encode loop is not
  blocked — records are dropped and counted.
- `network.rs`, two real actors over loopback: recording without the grant is
  refused before a file is opened; a granted recording lands under
  `recordings_dir()`, a second start returns the same path, both sides light
  up, and the flushed file replays with its start and stop events; a guest's
  request waits for the host, does not queue a second dialog, and records
  nothing by itself.
- `recording-ui.test.ts`: the button is unreachable without the grant; a press
  asks the core and shows nothing until the core says so; the panel shows the
  file name and not the path; Allow grants before it records; the guest's
  badge reads the flags byte and carries no way to dismiss it.
- `toolbar.test.ts`: the guest's button asks once and says only that it asked.
