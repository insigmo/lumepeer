# ADR 0067 — Codec negotiation: the guest advertises, the host intersects, H.264 is the floor

Status: accepted
Date: 2026-09-09

Foundation for the H.265 (backlog item 30), AV1 (31) and VP9/VP8 (32) batches.
Amends nothing; extends the `Hello`/feature-string machinery §9.1 and ADR
0024/0037/0060 already established, and the codec split
`crates/media/src/encode/mod.rs` already draws between H.264 (mandatory,
software fallback always available) and AV1 (optional, hardware-only, no
fallback).

## Context

Today there is exactly one codec, and negotiation does not exist: a host
always encodes H.264, and a guest that decodes for itself simply assumes the
bitstream it is handed is H.264 and reads `avcCodecString` out of the stream's
own SPS to configure `VideoDecoder`. That worked only because it was never
wrong — nothing in this workspace could produce anything else.

The three batches queued behind this one each add a second codec (H.265, AV1,
VP9). Each of them needs the same three things before it can ship anything:

1. A way for a guest to say which codecs it can actually decode, not which
   ones this build happens to know the name of.
2. A way for a host to say which codec it picked, so the guest is told rather
   than guessing — guessing is exactly what a second codec makes impossible:
   there is no single stream format left to sniff a codec out of blind.
3. A wire-compatible path for an old guest (built before any of this) to keep
   working against a new host, and for a new guest to keep working against an
   old host — §9.1's whole reason for existing.

Building this three times, once per codec batch, would mean three chances to
get the compatibility story wrong. This batch builds it once.

## Decisions

**The guest advertises what it can decode, not what it was built with.**
Three new `Hello.features` strings — `FEATURE_CODEC_AV1`, `FEATURE_CODEC_H265`,
`FEATURE_CODEC_VP9` (`"codec-av1"`, `"codec-h265"`, `"codec-vp9"`) — one per
optional codec. H.264 gets no string: it is the mandatory baseline every peer
can decode (`crates/media`'s software `openh264` fallback exists for exactly
this), so there is nothing to negotiate about it.

**The host intersects what the guest claims with what it can actually
encode right now, and announces the result.** A new tail-appended message,
`MessageKind::MediaCodec { codec: u8 }` (minor 11), sent once, before the
first frame of the media stream, and only to a guest that advertised at least
one of the three feature strings — proof that its build parses the new
discriminant at all, the same reasoning `FEATURE_MEDIA_UNAVAILABLE`'s own doc
comment gives for gating a message on a string rather than the minor alone. A
guest that advertised none of them is left exactly where it is today: H.264,
read out of the stream, no new message ever sent to it. This is what makes an
old guest and a new host, and a new guest and an old host, both work
unchanged.

`codec` is a plain wire byte (`0 = H264, 1 = Av1, 2 = H265, 3 = Vp9`), decoded
through a small `pub enum MediaCodec` in `crates/core::protocol` whose
`TryFrom<u8>` refuses anything else with `CoreError::Malformed` — the same
discipline `UnattendedRejection` and `MediaUnavailableReason` already apply to
their own closed sets, and the reason `MessageEnvelope::decode`'s
`check_limits` validates it at the same choke point every other payload is
checked at, rather than leaving it for whichever encoder or decoder happens to
`match` on it first.

**The intersection asks the same question `select_encoder` will ask again.**
`choose_media_codec` calls `lumepeer_media::encode::probe_hardware` for AV1
exactly as `select_encoder` does when it actually builds the encoder, rather
than keeping a second opinion that could disagree with it. H.265 and VP9 have
no encoder anywhere in this workspace yet — `lumepeer_media::encode::VideoCodec`
has no variants for them — so the host's own side of the intersection is
`false` for both, unconditionally, until batches 08/09 give it something real
to check. An empty intersection is H.264, always.

**This is deliberately not "we shipped with the feature so it must work."**
`.github/workflows/release.yml` carries a comment above its build matrix
recording the same shape of mistake from v0.0.14: a release shipped Windows's
hardware encoder (`encode-mf`) without its software fallback
(`encode-openh264`) alongside it, and a host with no hardware H.264 encoder
went permanently blank — nothing had confirmed a usable encoder actually
existed before the session relied on one. Both sides of this negotiation are
built to the opposite discipline: verify before you rely on it. The host's
half already had it (`probe_hardware` really probes). The guest's half is the
harder one, addressed next.

**The guest's real capability check cannot run before `Hello` is sent, so
`Hello` claims nothing new for now.** `view-decoder.ts` gets
`supportedOptionalCodecs()`: a real per-codec probe through
`VideoDecoder.isConfigSupported`, one profile per codec, never a table of "we
think this WebView can" — the same discipline the host's `probe_hardware`
already follows, applied to the guest side. But that function needs a
`VideoDecoder`, which is a page-scoped Web API, and the *view* window — the
only page a guest ever asks this question from — does not exist yet at the
point `connect_once` builds the `Hello` it is about to send: the window opens
only after `HelloAck` and consent, both of which come strictly after `Hello`
in this protocol's sequence. There is no window to ask.

Two ways to close that gap were considered:

- Route the answer through the process instead of the window: probe from
  whichever webview the app already has running (its main window) and thread
  the result down into `connect_once` before the dial. Rejected for this
  batch. It works, but it is an IPC surface `connect_once` does not have
  today — a new value has to reach the actor from a window before any session
  exists — and every one of this repo's other feature-gated messages instead
  keeps the split at "the guest is the process," never introducing a second
  moving part just to answer a question nothing can act on yet (see the next
  point). Revisiting this is fair game for whichever of 07/08/09 first needs
  it to matter.
- Advertise nothing extra and let the intersection fall back to H.264, exactly
  as if the guest had asked nothing at all. **Chosen.** It costs nothing today:
  no host build in this workspace can encode AV1 (no hardware backend reports
  it), H.265, or VP9 (no encoder at all) regardless of what a guest claims, so
  the host's own side of the intersection is `false` for all three either way.
  Advertising a guess the process has not verified would be the same
  unverified-capability mistake the previous section describes from v0.0.14;
  advertising nothing is the honest statement of what is actually known at
  that point in the handshake.

`connect_once` in `apps/desktop/src-tauri/src/network.rs` carries this
reasoning as a comment next to the feature list it builds, so it reads as a
decision rather than an omission. `supportedOptionalCodecs()` is implemented
and unit-tested now — real probes, real assertions — as exactly the piece
07/08/09 will need once probing has an actionable path into `Hello` worth
building: each of those batches introduces the first codec whose answer could
ever matter, and is the natural point to also decide how the real answer
reaches the handshake (a startup-time probe cached before any dial, or the
window-scoped IPC path this ADR declined to build ahead of need).

**The negotiated codec rides the existing per-chunk IPC header, not a new
command.** `view_next_chunk`'s fixed header
(`apps/desktop/src-tauri/src/view.rs`, `apps/desktop/src/view-decoder.ts`)
grows from 8 to 9 bytes, the ninth being the codec byte, on every answer
including an empty one. A separate "ask the negotiated codec" command was
rejected: the view window can call it before negotiation has actually
finished and get a stale answer — a race the header does not have, because it
is not a question asked out of band, it is a fact stated on every response the
window already has to read.

**A mid-session codec change is not supported — it gets the desync
treatment.** If the codec byte a window reads changes between two chunks, the
decoder is thrown away and rebuilt from the next intra frame, exactly like
`CHUNK_FLAG_DESYNC` already does for a redialled or discontinuous stream. This
batch never triggers that path itself (nothing renegotiates mid-session yet),
but the mechanism has to exist before a later batch's ABR-driven or
hardware-loss-driven codec fallback can rely on it.

**H.264 alone still reads its profile from the stream; the other three get a
fixed config string.** `avcCodecString` stays exactly as it is — the host's
hardware encoder picks its own profile (High, Main, Baseline) depending on
what the driver gives, so nothing else can name the right string for it. AV1,
H.265 and VP9 have no encoder anywhere yet, so there is no profile of its own
choosing to read back; each gets one fixed `VideoDecoder` config string
(`av01.0.04M.08`, `hev1.1.6.L93.B0`, `vp09.00.10.08` — Main/profile-0 baseline
levels) that batches 07/08/09 either keep or replace with a real derivation
once there is a real bitstream to derive one from.

## Consequences

Nothing observable changes for an existing session: every build in this
workspace still only ever encodes H.264, so `choose_media_codec` always
returns it, `MediaCodec` is never sent to a guest that never asked, and a
guest that did ask gets an explicit `MediaCodec { codec: 0 }` telling it so.
The wire, the IPC header and the golden vectors grow by exactly the width
this batch adds and not one byte more.

What is left open, on purpose: no codec other than H.264 can actually be
chosen until 07 (AV1), 08 (H.265) or 09 (VP9) lands, and the real
browser-verified path from a guest's actual `VideoDecoder` capability into its
own `Hello` is not built — only the mechanism it will plug into is. Each of
those batches inherits a negotiation that already knows how to fail safely
towards H.264; what they add is a codec the negotiation can actually land on.
