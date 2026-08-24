# ADR 0027 — The dial leaves the actor loop, and so does the frame poll

Status: accepted
Date: 2026-08-24
Extends: ADR 0026 (direct paths by default, and a release build that can be
diagnosed)

## Context

v0.0.16 was installed from the GitHub release on two Windows machines — one in
Spain behind `79.147.51.251`, one in Russia behind `85.173.133.81`, both
numbering their LAN `192.168.1.0/24` and both on the same tailnet. The report
was that the invite "does not arrive", and that when a session did come up the
app felt slow and switched between screens badly.

The transport was not the problem. `wan_probe`, the real stack with no UI,
connected between exactly those two machines in **130 ms** and settled on a
direct path with a 150 ms round trip:

```
GUEST path kind=direct selected=true rtt=129.1611ms remote=Ip(100.96.209.116:53155)
RESULT ok
```

The app's own release log said what had gone wrong instead:

```
20:58:28  issuing an invite  addrs={Relay(euc1-1), Ip(85.173.133.81:15070), … 8 in all}
20:58:44  WARN dropping an incoming connection that did not finish its handshake in time  timeout_secs=10
```

Three defects, none of them in the network.

**1. The host gave up on a guest that was still arriving.**
`CONTROL_HANDSHAKE_TIMEOUT_SECS` is documented as the deadline for the
`Hello`/`HelloAck` exchange — one round trip on a connection that is already
up. `spawn_handshake` wrapped it around `finish_accept` as well, so the same
ten seconds also had to cover the *QUIC* handshake, whose length is the far
side's hole punching. A guest gets a fifteen-second budget from iroh for that.
The host dropped the connection at ten. Both sides then reported a failure
neither had caused, which is the shape of failure ADR 0026 was written about,
one layer up.

**2. The dial ran on the actor loop.** `Actor::run` awaits
`handle_command`, and `ActorCommand::InviteConnect` awaited
`connect_with_ticket` — dial and handshake included. For as long as iroh kept
trying, the actor could not accept an incoming connection, deliver a
`ConsentGrant`, answer the four commands the webview polls every second, or
serve a frame to an open view window. A slow dial did not just fail slowly; it
froze everything else the app was doing, including any session already running.

**3. Nothing tried twice.** A ticket carries the address set the host had at
the moment it read the code out. By the time a human has pasted it the host may
have moved relay — this pair moves between `euc1-1`, `use1-1` and `aps1-1`
within minutes, which ADR 0026 already recorded — its NAT binding may have
changed, or its discovery record may not have propagated. iroh repairs all of
that by itself, but only if something dials again. The only thing that did was
the user, who cannot tell a stale address from a dead host, and who was told
"the host could not be reached" either way. Reproduced with the fix half in
place, which is what showed that retrying the dial alone was not enough:

```
host   21:36:41  issuing an invite  addrs={Relay(euc1-1), …}
host   21:36:45  home is now relay use1-1, was Some(euc1-1)
host   21:36:52  dropping an incoming connection that did not finish its control handshake in time
host   21:36:57  Lost connection to relay server: Ping timeout
guest  21:36:59  invite connect failed: stream i/o failed: connection lost
```

And one thing the fix for #2 exposed: `view_next_frame` went through the actor
mailbox too. The picture already lives in a `watch` channel that the media task
writes and nothing else mutates; it was routed through the actor only because
that was where the `input` grant lived. So every frame of every view window
queued behind whatever else the actor was doing — which at 30 fps is the
difference between a remote desktop and a slideshow.

## Decision

1. **Two deadlines on an incoming connection, not one.** Finishing the QUIC
   handshake gets `INCOMING_ACCEPT_TIMEOUT_SECS` (20 s), deliberately longer
   than a guest's own dial budget; the control handshake keeps
   `CONTROL_HANDSHAKE_TIMEOUT_SECS` (10 s) for the round trip it was always
   meant to bound. A guest that is still hole-punching has not gone silent.

2. **The dial runs off the actor loop.** `Actor::spawn_dial` validates
   everything it can decide without the network — the code parses, the ticket
   is well formed, this node is not already connected, no other attempt is in
   flight — and fails the IPC call synchronously if any of that is wrong.
   The rest runs on its own task and reports back as `ActorEvent::Dialed`, which
   the actor handles on its own thread, the only one allowed to store a
   connection. `handle_command` is no longer `async` at all, and that is the
   property to keep: the loop awaits it.

3. **A connect is retried `DIAL_ATTEMPTS` times** with `DIAL_RETRY_BACKOFF_MS`
   between attempts and `CONNECT_ATTEMPT_TIMEOUT_SECS` bounding each one, so a
   single attempt cannot consume the whole budget. Dial *and* handshake: this
   pair's failure is not a dial that never lands, it is a connection that comes
   up over the host's relay link and then loses it mid-`Hello`, which surfaces
   as `NetError::Io` — this side's observation that a stream stopped, not
   anybody's verdict. What is never retried is an answer: a bad ticket, a
   version mismatch or a refusal is a decision, and asking again only collects
   it twice.

4. **`ConnectPhase::Dialing`,** so the connect form can stay disabled from the
   click rather than from the moment the handshake lands, and
   **`connect_status` carries the §18 code** of a failure. The dial can no
   longer reject the IPC call that started it, so without the code every
   transport problem would reach the user as one undifferentiated sentence —
   the exact complaint ADR 0026 opened with. The webview owns the wording, in
   both locales; the classification is the same `classify_net` the error
   channel already used, so nothing is disclosed that was not disclosed before.

5. **The frame poll reads a shared feed instead of the mailbox.**
   `ActorHandle::view_frame` is synchronous and reads a `ViewFeed` — the
   picture's `watch::Receiver` plus an `AtomicBool` mirroring the live `input`
   grant — from a map the actor writes in `start_view` and `stop_view` and
   nobody else touches.

### The picture, measured

The same report said the app felt slow. Two of the causes are above — the
frozen actor and the frame poll behind its mailbox — and one is not, so it is
recorded here with numbers rather than left as an impression.

Measured on the pair above, guest polling `view_next_frame` from the view
window at `requestAnimationFrame` rate while driving the host's cursor so the
screen genuinely changes, optimized build both sides:

| | frames/s delivered | cost of one poll carrying a picture | cost of one poll carrying none |
| --- | --- | --- | --- |
| before | 3.8 | 104 ms | 5.1 ms |
| after | 5.6 | 118 ms | 5.8 ms |

The host is not the limit: it burns about one core, and the two encoder
settings changed here (`ScreenContentRealTime` instead of the camera-tuned
default, `Complexity::Low`, and threads asked for explicitly rather than left
at openh264's "auto", which was leaving it single-threaded) moved the delivered
rate but did not lift the ceiling.

The ceiling is the transfer. A 1080p picture is 8 MiB of RGBA, and moving it
across the Tauri IPC bridge costs ~112 ms — the difference between a poll that
carries pixels and one that does not, since the Rust side of both is a borrow
and a memcpy. That is ~70 MB/s, and it caps the view at roughly 8 fps no
matter what the host does. In the run above the guest spent 4.6 of 6 seconds
blocked inside `view_next_frame`.

Cutting that means sending fewer bytes, and the obvious way is to stop sending
RGBA at all: the decoder worker already has the picture as YUV420 and converts
it to RGBA *inside the sandbox* (`write_rgba8`), which is 2.67× more bytes
through the shared-memory ring, through the IPC bridge, and an extra
conversion. Handing the webview YUV420 and converting in a WebGL shader would
remove all three. It is not done here: it crosses the sandbox boundary
(§11.3, ADR 0005), changes `SLOT_PAYLOAD_BYTES` and its compile-time
assertion, and replaces the canvas painter — none of which belongs in the same
change as a connectivity fix. It is the next thing to do, and the numbers above
are what it should be measured against.

## Consequences

- A connect attempt no longer stops the app. A session already running keeps
  its picture and its input while another dial is in flight, and the host half
  of the same app keeps accepting guests.
- The user is told which failure it was: unreachable, expired code, this device
  not ready yet, or an incompatible version, instead of "could not connect".
- Worst case, an attempt now takes about three times as long before it is
  reported as failed. That is the right trade only because it no longer costs
  the user anything to wait — nothing else is blocked while it runs — and
  because the attempt that succeeds is frequently the second one.
- The host holds a handshake slot for up to 30 s rather than 10 s in the worst
  case. `MAX_INFLIGHT_HANDSHAKES` still bounds the concurrency at 8, and each
  waiting task is a timer and a connection, so the ceiling this widens is
  memory that was already bounded — not a new class of exposure.
- §2.3 is unchanged. `ViewFeed` holds a copy of a decision `lumepeer-core`
  made, written only by the actor; a reader can observe it and nothing more.
  The entry is removed before the window is told to close, so a poll racing a
  revoke reads either the live grant or nothing at all.
- The software encoder trades quality per bit for frame rate. On a host with a
  hardware MFT nothing changes: `select_encoder` still prefers it, and this
  configuration is only ever reached as the fallback ADR 0026 added.
- What this does **not** fix is the relay link itself. The host of this pair
  loses its connection to `euc1-1` to a ping timeout roughly every twenty
  seconds, indefinitely, exactly as ADR 0026 recorded — the retry makes a
  session survive that, and a direct path avoids it, but a client that can only
  reach its peer through that relay is still at the mercy of it. Pointing both
  ends at a relay they can hold (`[network].relay_url`, `LUMEPEER_RELAY_URL`,
  docs/relay-deployment.md) remains the answer for a deployment where hole
  punching cannot succeed.

## Verification

- `wan_probe` between the two machines above, and the installed clients driven
  headlessly through `tauri-pilot` (`invite_create` → `invite_connect` →
  `session_grant`), completing invite → consent → view window.
- Ten invite → connect → grant → revoke cycles between the two machines, all
  ten landing, `invite_connect` returning in 53–77 ms every time — the number
  that used to be the whole dial.
- `a_dial_in_flight_does_not_stall_the_actor` holds a dial open against a host
  that has gone away and asserts every other command still answers — the
  regression test for the defect that had no test at all.
- `the_connect_phase_waits_for_the_host_to_decide` now covers both waits, the
  dial and the decision; the webview suite covers the phase keeping the button
  disabled, each failure code reaching its own sentence, an unknown code
  falling back to the generic one, and a new attempt clearing the last
  failure's message.
