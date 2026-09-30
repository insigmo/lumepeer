# ADR 0123 — A host with a controlling guest does not go idle, and a dead path is noticed in eight seconds

Status: accepted
Date: 2026-09-27

**Report.** "I held the connection open for a long time on purpose, and now I
can do nothing and the top says *Connection lost, reconnecting…*. Make it not
break, and when it does, come back fast."

Extends [ADR 0116](0116-the-rendezvous-pushes-through-nostr-relays.md)'s
account of `beta`'s network dying on input, and tightens
[ADR 0052](0052-serverless-transport-obfuscated-quic-with-stun-discovery.md)'s transport
timing a second time after ADR 0116.

## What was measured

The pair was `win` (guest) and `beta` (host) on the obfuscated transport, with
both app logs and `beta`'s System log lined up. Two different failures were
in them.

**One long outage.** The session had been open and idle for an hour. At
14:06:36 UTC `beta` logged Kernel-Power 566 ("the system session has
transitioned … `Reason InputHid`"), which is the guest's first mouse move in
an hour reaching it. From that second `beta` reached nothing: every relay
connect timed out, every pkarr publish failed at TCP connect. Its
`SessionUnlock` followed at 14:09:49, and the session resumed at 14:09:31, two
seconds after the host's network was back. ADR 0116 already knew this much and
concluded that no software on the far side can carry a session through it.
That stays true. What it did not try is not going idle in the first place — it
held a power request (`ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED`), which keeps
the display lit but not the idle timer from running.

Three things measured this time, on `beta` with a script in the signed-in
session and no lumepeer involved:

- A zero-length relative `SendInput` mouse move resets `GetLastInputInfo`
  (208 s → 0.06 s) and leaves the cursor where it is.
- A 566 follows *every* input that ends an idle spell, but the network does
  not die every time: after the guest's input at 12:37:57 and at 12:51:03 UTC
  the session carried on untouched, and at 14:06:36, 14:27:52 and 14:34:28 the
  whole network went.
- **Once it has gone, it stays gone for as long as the display stays on.** A
  wake with nothing after it: network back 2 min later, `SessionUnlock` at
  3 min. The same wake followed by a keep-awake every 30 s: the network stayed
  dead for twelve minutes, until 4.5 min after the last of them. So a
  keep-awake that runs in that state is not neutral, it is the longest outage
  there is.

**Short drops every few minutes.** Nine of them between 12:08 and 14:13 UTC, all
on the direct obfuscated path, none on a 566. Each one had the same shape: the
host declared the guest gone after `QUIC_MAX_IDLE_TIMEOUT_SECS` (20 s) of
silence; the guest's picture stopped about then, and its own control connection
timed out 15-50 s after that. The resume that followed took one to five
seconds. All of the frozen time was noticing, none of it was reconnecting.

## 1. The host keeps its desktop awake for a guest who can wake it

While at least one active session may inject input (`FullControl` with the
input grant, or `ControlLimited` with a non-empty allowlist —
`SessionManager::someone_may_inject`), a Windows host checks every
`HOST_KEEP_AWAKE_SECS` (15 s) how long its desktop has been idle and, **only
if that is between `KEEP_AWAKE_MIN_IDLE_MS` (10 s) and
`KEEP_AWAKE_MAX_IDLE_MS` (50 s)**, performs a zero-length relative mouse move.

- **Never past the upper bound.** A desktop idle that long may already have
  turned its display off, and the keep-awake would then be the wake. 50 s is
  under the one-minute floor of Windows' display and screen-saver timeouts.
- **Never below the lower bound.** Somebody's input that recent keeps the
  machine awake by itself; and if it was the wake that took the network, more
  input is what keeps the network down. Such a session parks within the new
  8 s idle timeout, and a parked session is not kept awake at all, so 10 s
  covers the whole window.
- Between the two, the check lands the move by 25 s of idleness at the latest,
  so a machine that is awake stays awake.

It is performed where a guest's input is: through the `LocalSystem` desktop
injector first (ADR 0114; new descriptor kind `KeepAwake`, byte 5), because
UIPI drops the host's own `SendInput` in front of a `VMware` window, and by the
in-process injector when that is not there. The idle check runs where the move
is made, so both measure the console session's own clock. Off Windows nothing
is done.

## 2. A dead obfuscated path is noticed in eight seconds

`QUIC_KEEPALIVE_SECS` goes from 5 to 2 and `QUIC_MAX_IDLE_TIMEOUT_SECS` from 20
to 8.

- The idle timeout is the frozen picture. Two keep-alives in a row may still be
  lost without closing a healthy path (asserted: 3 × 2 < 8).
- Both ends of a path that dies notice within one keep-alive of each other, and
  the guest's first resume goes out `RESUME_RETRY_SECS` (3 s) after its own
  notice, so it reaches a host that has already parked the session rather than
  one that would refuse the claim (asserted: 2 < 3).
- QUIC takes the smaller of the two peers' idle timeouts, so one updated end is
  enough for the timeout; each end sends its own keep-alives.

The shape measured above goes from about 45 s of frozen picture to about 20:
8 s for the host, 8 s more for the guest, 3 s to the first resume, and the
resume itself.

## Consequences

- While a guest who may drive it is connected, a Windows host's display does
  not turn off and its screen saver and idle lock do not start. A view-only
  guest does not hold it awake: it sends no input, so it wakes nothing.
- A host that was already idle when the guest arrived is not protected: the
  guest's first input is the wake, and on `beta` that may still cost the
  network. The keep-awake never wakes anything, so what it removes is the
  repeat within a session — the report — not the first one. For `beta` itself
  the fix is on the machine, where ADR 0116 pointed: the Realtek 8852BE's
  power management ("Allow the computer to turn off this device"), the IObit
  "Driver Booster Power Plan", or the unused wired port.
- A Wi-Fi stall longer than 8 s now drops and resumes, where it used to freeze
  and carry on. The resume costs one to five seconds and a key frame.
- A service older than this refuses the `KeepAwake` descriptor like any unknown
  kind (one warning in the service log per check), and the host keeps the
  desktop awake in-process instead — which does not reach past a `VMware`
  window, until the service is updated too.
- Not addressed here: a resume that arrives before the host has noticed the
  drop (the host-to-guest direction dying first) is still refused (ADR 0089),
  and a resume attempt during a long outage still spends half of each cycle on
  the iroh leg of the dial plan, so a host that comes back can wait up to one
  cycle to be dialed.
- Not yet shown end to end on `beta`: a session kept awake this way, left idle
  past the display timeout, and then driven again. The measurements above are
  of the mechanism, not of this build.
