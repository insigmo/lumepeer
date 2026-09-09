# ADR 0070 — The guest asks its WebView before it says Hello

Status: accepted
Date: 2026-09-09

Completes [ADR 0067](0067-codec-negotiation-guest-advertises-host-intersects.md),
whose guest half was left unwired. Needed by
[ADR 0069](0069-av1-asks-each-backend-and-va-api-cannot-answer.md), which gave
a host something other than H.264 to choose.

## Context

ADR 0067 built codec negotiation in both directions and then advertised
nothing. Its own comment in `connect_once` says why:

> No `FEATURE_CODEC_AV1`/`FEATURE_CODEC_H265`/`FEATURE_CODEC_VP9` here. The
> real answer — `VideoDecoder.isConfigSupported`, asked by
> `view-decoder.ts`'s `supportedOptionalCodecs()` — can only run inside the
> view window, and the view window does not exist yet at this point.

That was a correct refusal to guess, and it cost nothing while no host could
encode anything but H.264 anyway. ADR 0069 ends that: a Windows host with AV1
hardware can now encode AV1, and would still never be asked for it, because
`GuestCodecSupport::understands_negotiation` is false for every guest this
workspace builds. The negotiation would have been dead code with a test suite.

The premise that needed re-examining is "only the view window can answer".
The claim is true of the *view window*, not of `VideoDecoder`. `WebCodecs`
lives in the WebView runtime, and both windows this application creates are
`WebView2` (or `WebKitGTK`, or `WKWebView`) instances of the same runtime in
the same process, configured identically. The main window exists from startup.
It can be asked.

## Decision

**The main window probes, once, at startup, and tells the actor.** `main.ts`
calls the existing `supportedOptionalCodecs()` — one
`VideoDecoder.isConfigSupported` per optional codec, the browser's own answer,
never a table of assumptions — and passes the resulting `MediaCodec` wire
bytes to a new IPC command, `report_decoder_codecs`. The actor keeps them in
`own_codec_support`, and `connect_once` appends the matching feature strings
to the `Hello` it is about to send.

**It stays a probe, not a table.** The thing ADR 0067 refused was advertising
a codec nobody had asked the browser about. That refusal is intact: the answer
still comes from `isConfigSupported`, still one config string per codec, still
inside a `WebView`. What changed is *which window* asks, and the window is not
what makes the answer trustworthy.

**Nothing reported means nothing advertised.** The field starts empty. A dial
that happens before the report lands — the actor's own tests, a build with no
window, a user who hits Connect inside the first frames of startup — sends a
`Hello` with no codec strings in it, which puts that session exactly where
every pre-ADR-0067 guest is: H.264, and no `MediaCodec` message sent to it at
all. There is no failure mode here that is not simply "the old behaviour".

**A byte this binary cannot name is dropped, not refused.**
`GuestCodecSupport::from_wire_bytes` keeps the bytes `MediaCodec` assigns and
ignores the rest. An unknown byte means a webview bundle newer than the binary
loading it, and the honest answer from a binary that has never heard of that
codec is "no": it could not name the codec in a `Hello` even if it wanted to.
`MediaCodec::H264` is ignored for the same reason it has no feature string —
every peer decodes it, and there is nothing to advertise.

**The command carries no authorization.** `report_decoder_codecs` is a main
window command like the rest, checked by `check_window`. What a lying webview
achieves is making its own guest ask a host for a codec it cannot then decode,
and the guest is the only side that pays: the host encodes what it was asked
for, and the window shows nothing until the user reconnects. §2.3's rule is
that the UI decides nothing that matters to anyone else, and this decides
nothing that matters to anyone else.

**One three-bit set, read in two directions.** `GuestCodecSupport` was the
host's record of what a peer advertised. It is now also a guest's record of
what its own webview reported, with `from_wire_bytes`/`features` next to
`from_features`. A separate type for the guest side would be the same three
booleans with the same meaning, and the round trip — what a guest advertises
is what a host reads back — is worth being a single type that a test can
exercise end to end.

## Consequences

A guest on a machine whose WebView decodes AV1 now says so, and a host with
AV1 hardware can act on it. That is the path ADR 0069 built and ADR 0067
defined, joined up. H.265 and VP9 travel the same path the day an encoder
exists (batches 08/09); nothing further is needed on the guest side for them
than an entry in `OPTIONAL_CODEC_CONFIGS`, which is already there.

The report is per process, not per connection. A guest that dials two hosts
advertises the same list to both, which is correct: it is one webview runtime.

`connect_once` gained a parameter and `spawn_dial` copies the field out on the
actor's own thread before spawning the dial task. What a `Hello` advertises is
settled when the dial starts, rather than read later from a field the actor is
still free to change.

## Still open

- The probe runs in the main window and the decoding runs in the view window.
  They are the same runtime, and nothing observed says otherwise — but nothing
  has *verified* it either, because no machine with hardware for an optional
  codec has been reached (ADR 0069's "Still open"). If a platform is ever
  found where the two windows answer differently, the fix is for the view
  window to report again on open and for the host to be told, which is a
  second message and a bigger change than this one.
- A guest whose WebView gains a codec after startup — a browser-engine update
  applied under a running process — keeps advertising what it reported at
  startup until it is restarted.
