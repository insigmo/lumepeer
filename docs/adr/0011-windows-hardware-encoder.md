# ADR 0011 — Windows Media Foundation hardware H.264 encoder

Status: accepted
Date: 2026-08-17

## Context

§19 phase 4 asks for hardware video encoding; §11/§18 say hardware first,
the `openh264` software fallback second. `crates/media/src/encode/mod.rs`'s
`probe_hardware` returned `None` unconditionally on every platform until
now — real encoder hardware existed on this development machine, so unlike
the platforms ADR 0007 records as genuinely unbuildable here (macOS,
Wayland's PipeWire path at the time), there was no excuse for Windows
hardware encoding to stay a documented gap.

## Decision

`crates/media/src/encode/windows.rs`, behind a new `encode-mf` feature
(`crates/media/Cargo.toml`, matching the `capture-x11`/`encode-openh264`
convention: `cargo build --workspace` still needs no platform SDK by
default), drives a hardware H.264 encoder MFT through Media Foundation:

- `probe_hardware` on Windows actually enumerates and activates a
  hardware-flagged encoder MFT (`MFTEnumEx` with `MFT_ENUM_FLAG_HARDWARE`)
  and negotiates real input/output types before reporting
  `Some(EncoderKind::Hardware)` — the same activation path
  `MediaFoundationEncoder::new` then uses, so probing cannot claim
  availability construction fails to back up. AV1 is not implemented by this
  backend; `probe_hardware` reports `None` for it regardless of what the
  H.264 probe found, so §11's mutual-hardware-support rule for AV1 cannot
  be satisfied by a check that never actually looked for AV1 hardware.
- Hardware encoder MFTs are documented as always asynchronous, unlike the
  synchronous software MFT `MFT_ENUM_FLAG_HARDWARE` filters out. The module
  drives the documented async protocol: unlock `MF_TRANSFORM_ASYNC`, then
  only call `ProcessInput`/`ProcessOutput` in response to
  `METransformNeedInput`/`METransformHaveOutput` events from the
  transform's `IMFMediaEventGenerator`, bounded by
  `ENCODE_HW_EVENT_TIMEOUT_MS` (§14) so a stalled driver fails one
  `encode()` call instead of hanging the session.
- `unsafe impl Send for MediaFoundationEncoder` is sound because hardware
  H.264 encoder MFTs are documented free-threaded ("agile"); COM is
  initialized multithreaded-apartment on every entry point rather than
  once at construction, so a `Send` hand-off to a different thread cannot
  leave that thread outside the MTA.
- This is the second module in the crate allowed `unsafe_code` (after
  `decode::shm`, ADR 0005): every `IMFTransform`/`IMFSample` call in the
  `windows` crate's Media Foundation bindings is `unsafe fn` because it
  crosses into COM. Every block carries a `SAFETY:` note per §21.

### What running on real hardware caught that a clean compile did not

Two protocol bugs only surfaced once this ran against a genuine hardware
encoder MFT on this machine, not from reading the MSDN docs or from a
`cargo build`/`clippy` pass:

1. **Every `encode()` call hung until timeout.** The first
   `METransformNeedInput` arrived and `ProcessInput` was accepted, but the
   transform then emitted a second `METransformNeedInput` instead of
   `METransformHaveOutput` — it wanted more buffered input before handing
   back anything, which the trait's one-frame-in/one-frame-out contract has
   no way to supply. `MFT_MESSAGE_COMMAND_DRAIN` (MSDN's documented way to
   tell an MFT to flush whatever it can produce from the input queued so
   far, rather than waiting for its full pipeline depth to fill) sent right
   after `ProcessInput` fixed this. Read from the docs alone, this looked
   optional; empirically, without it every hardware call timed out.
2. **Changing the negotiated frame size on a live transform failed with a
   bare `E_FAIL`.** `SetOutputType` at a new width/height, on a transform
   that had already streamed at a different size, was refused outright by
   the driver on this machine. Media Foundation's dynamic-format-change
   path (`MF_E_TRANSFORM_STREAM_CHANGE`) is driver-initiated, not something
   an app can force by calling `SetOutputType` again — so `reconfigure`
   (and, by extension, any resolution change) now activates a fresh
   transform at the new size instead of trying to reconfigure the live one.
   A resolution change is a rare event (a screen resolution change, not a
   per-frame one), so the cost of re-activating is not a real concern.

Both are recorded here rather than only in a commit message because they
are the kind of "looked right in review, wrong against real hardware"
mistake §24.5 asks to degrade towards safety and document, not bury.

### What is still not covered

Bitrate changes rebuild the negotiated output type rather than going
through `ICodecAPI::SetValue`, the same trade-off ADR 0005 accepted for
`openh264` — a genuinely live bitrate change would need a `VARIANT`-based
`ICodecAPI` call this module does not build. `VideoToolbox` (macOS),
`MediaCodec` (Android) and VA-API (Linux) hardware encoders remain
unimplemented; `probe_hardware` stays `None` on every platform but Windows.

## Consequences

`select_encoder` now genuinely prefers hardware on Windows when one exists,
verified end to end (construct, encode multiple frames including a
resolution change, change bitrate) against real hardware on this
development machine — not just compiled. Elsewhere, hardware encoding
remains the documented gap ADR 0007 and the phase 4 status already track.
