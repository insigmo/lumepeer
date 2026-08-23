# ADR 0023 — Phase-7 catch-up: unattended access, address book, chat,
# clipboard, audio, session recording, relay deployment

Status: accepted
Date: 2026-08-22

## Context

The feature catch-up list (repo `questions.md`, opened against the
TeamViewer/AnyDesk/RustDesk gap list) left seven open decisions. Each was
answered by the project owner in `questions.md` and is now implemented; this
ADR records the decisions and their consequences in one place, per the
"never silently" rule.

## Decisions

### 1. Unattended password hashing: Argon2id (`argon2` crate)

Added as a workspace dependency (`argon2 = "0.5"`, RustCrypto). The device
password lives only as an Argon2id PHC string in the OS keystore
(`crates/core/src/unattended.rs`); verification happens in the TCB, and five
consecutive failures lock verification for 300 s
(`UNATTENDED_MAX_FAILED_ATTEMPTS`, `UNATTENDED_LOCKOUT_DURATION_SECS`). The
salt comes from the workspace CSPRNG via `SaltString::encode_b64`, which also
dodges the two-`rand_core`-versions conflict around feeding `rand` straight
into `SaltString::generate`.

### 2. TOTP second factor: own minimal implementation

RFC 6238 over HMAC-SHA1 with 6-digit codes (~50 lines,
`unattended.rs::Totp`), verified against the RFC's Appendix B vectors — the
published 8-digit values truncated to their last six digits. SHA1 stays
confined to authenticator-app compatibility (ADR 0021 note); `hmac`/`sha1`
are workspace dependencies used nowhere else. Verification accepts ±1 step
for clock drift.

### 3. Address book: plain JSON, keyed by base32 NodeId

`crates/core/src/address_book.rs`. Trust is per-`NodeId`, never per-label; a
corrupt file is an error, never a partially-trusting book. No secrets live in
the file (a NodeId is a public key).

### 4. File transfer: chunked pipeline over `rd/file/1`

`crates/net/src/file_transfer.rs`: strictly sequential chunks with a resume
point, BLAKE3 verified before anything leaves staging, concurrent-transfer
ceiling mirroring `MAX_PENDING_FILE_OFFERS`, length checked before
allocation on both ends. Control messages `FileAbort`/`FileChunkAck`
appended to the protocol at `PROTOCOL_MINOR` 1.

### 5. Audio codec: Opus via the `opus` crate behind `audio-opus`

§5.1 names Opus but no crate. Decision: `opus = "0.3"` whose `audiopus_sys`
vendors libopus and builds it **with cmake** — the same vendored-build
precedent as openh264-sys, so the default build still needs no platform SDK.
Feature-gated as `audio-opus` in `lumepeer-media`; without the feature every
audio entry point refuses loudly instead of passing silence. Parameters are
fixed in constants (48 kHz, stereo, 20 ms frames, 96 kbit/s default).
Packet loss decodes to concealment, never to an error (§24.5). Build hosts
need `cmake` on PATH once (documented here; CI images need it added when the
feature is enabled there).

### 6. Chat and clipboard: protocol first, actor-owned state

Both ride existing control-channel messages of `PROTOCOL_MINOR` 1
(`Chat`, `ClipboardSync`) with per-message limits enforced in decode
(allocation-DoS defense at the parse boundary). Session state — bounded
transcripts (`MAX_TRANSCRIPT_ENTRIES`), clipboard echo suppression — lives in
the desktop actor and dies with the connection; nothing persists (§15).
Clipboard sync is gated on the existing independent grants
(`clipboard_read` host-side, `clipboard_write` guest-side); chat needs no new
grant because it carries no control over the host.

### 7. Session recording format: custom append-only container ("LMRC")

Per the owner's decision: no external dependency, MKV export later.
`crates/media/src/record.rs` defines it: 8-byte header (magic `LMRC`,
version, reserved flags), then a sequence of timestamped records (video
bitstream / Opus packet / event-log JSON line), each length-checked before
allocation. Timestamps are relative to the first record (less metadata on
disk, §15). Append-only by construction: an interrupted session leaves a
valid prefix that still replays; torn tails are reported, not fatal.
Recording remains subject to the independent `recording` grant.

### 8. Relay: documentation + compose file, no custom relay code

`docs/relay-deployment.md` + `deploy/docker-compose.yml` for self-hosting
the official `iroh-relay` image (ACME TLS, ports 80/443). Additionally a
client-side override landed: `LUMEPEER_RELAY_URL` swaps iroh's default relay
fleet for a self-hosted one at endpoint bind time; a malformed URL is logged
and ignored rather than blocking startup.

## Protocol version

All wire changes (`Chat`, `KeyframeRequest`, `CursorShape`, `MonitorsList`,
`MonitorSelect`, `PrivacyMode`/`PrivacyModeAck`, `AudioStart`/`AudioStop`,
`FileAbort`, `FileChunkAck`, plus decode-side limit checks) are appended at
the end of `MessageKind` under `PROTOCOL_MINOR` 1; discriminants of existing
variants are untouched, so MINOR 0 peers keep decoding. Golden vectors were
frozen for the new messages in the same change (tests/interop).

## Consequences

- `cargo build --workspace` stays SDK-free; `cmake` becomes required only
  where `audio-opus` is enabled.
- Recording output is not yet player-consumable without the future MKV
  exporter; the format doc above is the contract for it.
- Clipboard sync currently covers UTF-8 text only; image/file payloads stay
  out of scope (§9.2 v1) and are refused by validation, not silently
  dropped.
