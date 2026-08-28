# ADR 0028 — Remote SAS, monitor choice and the guest microphone

Status: accepted — reconstructed from the code on 2026-08-28
Date: 2026-08-28 (this record). The decisions themselves shipped 2026-08-25 in
commit `6922b07`, which added `crates/media/src/sas.rs`,
`crates/media/src/playout.rs`, `STREAM_MIC` and the `SasRequest`/`SasAck` wire
messages in one pass.

## Reconstruction note

This ADR was never written when the change landed, and the number was used
anyway: roughly sixty doc comments across `crates/media`, `crates/net`,
`crates/core` and `apps/desktop/src-tauri` cite "ADR 0028", and
`crates/core/src/protocol.rs` names this file *by path* in its `PROTOCOL_MINOR`
3 note. The number is therefore fixed and cannot be reassigned.

What follows is reconstructed from those comments and from the code
(`docs/tasks/15-docs-and-adr-debt.md`, task 1). The module headers of `sas.rs`,
`playout.rs`, `capture/mod.rs` and `network.rs::on_mic_toggle` state their own
reasoning at length, so most of this is transcription rather than inference.
Where the code does not record *why*, this file says so instead of inventing a
plausible motive.

## Context

Design doc §11 collects the features that sit on top of a working session —
Ctrl+Alt+Del delivery, a choice of which of the host's monitors to watch, audio
in both directions. They arrived together as the guest view window's toolbar,
and they share one property that makes them worth a single record: each is a
control the *guest* presses that reaches into the *host's* machine, so each
needs an answer to "which grant does this ride on, and what does the guest see
when the host cannot do it at all?"

Three of them cannot be answered on every platform:

- Ctrl+Alt+Del is not a key an application may synthesize. `SendInput` with the
  SAS combo is filtered by UIPI; the sequence belongs to Winlogon. Only Windows
  has a supported path at all.
- Monitor enumeration exists on Windows (DXGI) and on X11 (RandR). A Wayland
  portal session hands back exactly one stream, chosen by the user in the
  portal's own dialog, and no protocol lets the application move it. macOS has
  no enumeration wired up.
- Audio playback needs a per-platform render API; macOS has none implemented.

The failure mode to avoid is the one §18 names: a control that looks live,
does nothing, and says nothing.

## Decisions

### 1. SAS goes through `sas.dll`, and the answer comes back on the wire

`crates/media/src/sas.rs` calls `SendSAS(FALSE)` from
`Win32::Security::Authentication::Identity` — the same entry point the Remote
Desktop client uses. `FALSE` names the calling process's own session, which is
the one the host user is sitting at. The `unsafe` block carries the
justification standard ADR 0012 set for `SendInput`: a documented Win32 entry
point with no invariants beyond its argument, and no safe binding.

Whether the call is *permitted* is not observable without making it: the OS
grants the right to a service in session 0 (the `SoftwareSASGeneration` policy)
or to an elevated process in the user's session, and an unelevated process gets
a silent no-op from the OS itself. So:

- `sas_available()` answers the *platform* question only, and answers it
  optimistically on Windows. The guest's button stays enabled there.
- The host answers every `SasRequest` with `MessageKind::SasAck { delivered }`.
  A platform with no SAS mechanism, a session without a live `input` grant, and
  a call the OS refused all produce an honest `false` the guest can show —
  never a log line the pressing user cannot see.

Off Windows `sas_available()` is `false` and the toolbar grays the button out
rather than letting someone press it into a dead end.

### 2. SAS is an input event, and is gated as one

`SasRequest` is acted on only from a session whose `input` grant is live at
that moment — the same per-event re-check every injected key gets (§8.1).
`network.rs::on_sas_request` refuses on the guest side too, by running the
request through `SessionManager::authorize_input` with a synthetic press, so a
guest whose role was lowered mid-flight does not even put the message on the
wire. The host's check is the authoritative one; the guest's only saves a round
trip.

On the wire this is `PROTOCOL_MINOR` 3, appended after `MediaUnavailable`, and
the host sends `SasAck` only to a guest whose `Hello` advertised
`FEATURE_REMOTE_SAS` — an older peer would read the unknown discriminant as
malformed and close the connection (§9.1).

### 3. Monitor ids are the platform's own enumeration order, and the list never lies

`CaptureTarget::Display(u32)` indexes the platform backend's own enumeration,
and `HostMonitor.id` is that same index. `MonitorsList` announces them;
`MonitorSelect` passes one back and the host restarts the capturer on it, with
the encode loop picking the new geometry up on its next frame exactly as it
would after a resolution change. Selection is gated on the `view` grant, and
the id is range-checked against the host's own monitors — a guest cannot name a
display that was never announced.

Three answers, one contract — "these are the ids `Display` indexes":

- **Windows / X11**: the real list, in enumeration order.
- **Wayland**: always exactly one entry. This is not a gap to be filled later.
  A portal session grants one stream and `MonitorSelect` cannot move it, so
  announcing three monitors would promise a choice `CaptureTarget::Display` has
  no way to honour.
- **Any platform with no enumeration** (macOS today): one primary entry rather
  than an empty list, because an empty list reads as "this host has no screens"
  (§18: degrade honestly, never lie).

### 4. The guest microphone is an input surface

The reverse audio direction — the guest's voice reaching the host — is gated on
the live `input` grant, not on `view` and not on a grant of its own. The
reasoning is recorded in `on_mic_toggle`: the mic feeds *into* the host's
machine, so a view-only guest may watch but not speak. It is re-checked at
toggle time on the guest and per request on the host.

The stream rides the media connection the picture already dialed (§4.1) rather
than dialing its own; a press before there is a picture is refused. It
announces itself with its own first-byte tag, `STREAM_MIC` (`M`), distinct from
the host's outbound `STREAM_AUDIO` (`A`), so a host can never confuse a stream
a guest opened with one of its own.

### 5. Playback mirrors capture, and a platform without it refuses loudly

`playout::AudioPlayer` is the exact mirror of `capture_audio::AudioCapturer`:
`start` / `push(one wire-format chunk)` / `stop`, blocking-push, with the wire
format fixed by constants (48 kHz s16 stereo, §11) so nothing negotiates per
session. Conversion to the device's own mix format is linear resampling plus
channel mapping, kept local as the inverse of `to_wire_pcm`, and a silent chunk
is real silence so gaps in the sender's clock do not click.

Two backends: WASAPI shared-mode render on Windows, PipeWire on Linux behind
the same `audio-capture-pipewire` feature that carries the capture direction —
it is the same binding set. macOS gets a refusal from `platform_player()` and
runs without guest audio, saying so (§18).

## Consequences

- Ctrl+Alt+Del works only against a Windows host, and only one the user
  launched elevated. That is a property of the OS, not of this design, but it
  means the toolbar's most "administrative-looking" button is the one most
  likely to answer `false` in the field.
- The mic riding the `input` grant means a host that grants `view` alone gets a
  session it can watch and be watched in, with no voice from the guest. This is
  deliberate and is the conservative reading; if it later proves wrong, the fix
  is a new independent grant, not widening `view`.
- Wayland hosts appear to the guest as single-monitor machines even when they
  are not. The monitor picker shows one entry and is inert, which is the honest
  rendering of what the portal actually granted.
- Every one of these controls is reachable only from the view window
  (`check_view_window`), so the main window's IPC surface did not grow.
- `SasAck` and the `M` stream tag are additive: peers that predate them never
  receive either, by feature string and by never opening the stream.

## What this record cannot say

The code does not record why the SAS answer was made a wire ack rather than an
IPC-local result, beyond `sas.rs`'s remark that "the wire answer is an ack the
guest can show, not a log line". The choice of `input` as the mic's gate is
argued in the code; whether a separate `microphone` grant was considered and
rejected is not written down anywhere, and this file does not claim it was.
