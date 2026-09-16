# ADR 0088 — The logon screen is a capture target, and a switch is a state

Status: proposed — §5 reverses one sentence of ADR 0085 and waits for a human
to accept that
Date: 2026-09-16

Pack `26` (`docs/gap-tasks/26-session-zero-logon-screen.md`). Builds on
[ADR 0085](0085-the-host-can-be-a-service-and-the-screen-belongs-to-an-agent.md)
and [ADR 0087](0087-assembling-the-session-zero-host.md). ADR 0085 left out
"the logon screen … and following a fast user switch", and until now a
session-0 host with nobody signed in served control and no picture. This ADR
is about what serving that screen, and moving between screens, takes.

## Context

A machine people sign in and out of has three possible sources of a picture:
nothing, the logon screen (`WinSta0\Winlogon`), and a signed-in user's desktop
through their agent. ADR 0085 built the third. Two things were missing.

- **The logon screen had only a per-frame worker.** ADR 0056 launches a
  `LocalSystem` process onto `Winlogon` for one GDI snapshot and then ends it.
  That suits a UAC prompt. It does not suit somebody typing a password: every
  frame costs a process launch, and the 500 ms throttle
  (`SECURE_DESKTOP_CAPTURE_INTERVAL_MS`) is too slow to watch your own
  keystrokes through.
- **Moving between sources had no state of its own.** The supervision loop
  polled `WTSGetActiveConsoleSessionId` on a tick. For up to a tick after a
  fast user switch it still believed in the old session, and the frame mapping
  still held that session's last frame. That frame is another person's desktop.

## Decisions

### 1. The logon screen gets a worker that lives as long as the screen

`LumepeerHost` launches `lumepeer-service.exe --logon-screen-worker` onto the
console session's `WinSta0\Winlogon`, as `LocalSystem` with its token stamped
into that session (`crates/service/src/logon_screen.rs`). The worker:

- attaches to **the same agent channel** and speaks **the same closed command
  list** (`agent_protocol.rs`) as a session agent. Its pipe access list has no
  user on it (`SYSTEM_ONLY_SDDL`), and the host applies the same pid check;
- captures on a tick of `LOGON_SCREEN_CAPTURE_INTERVAL_MS` (200 ms) into its own
  mapping, `FrameChannel::LogonScreen`, which only `LocalSystem` and
  administrators can open. 200 ms is a deliberate choice. The screen is nearly
  static, so there is no reason to pay for video rates. But at fewer than five
  frames a second, a person typing a password can't see their keystrokes;
- performs the input it is forwarded with the existing ADR 0057 code
  (`secure_desktop_input::perform`), from a process already on `Winlogon`
  rather than one launched per key.

Three properties do **not** change:

- The worker's command list is still closed: no path, peer, grant or program.
- It is still launched by the privileged side from a constant.
- **Input is still gated by `secure_desktop_input`.** The actor checks that
  grant whenever `ViewWindows::on_secure_desktop()` is true, the same way it
  checks it when a desktop client's capture is blocked by UAC. On a service
  host the event then goes through the ordinary injector to the worker, not to
  the helper. The helper refuses session-0 callers by design (ADR 0049).

What **does** change is that a `LocalSystem` process now stays on the secure
desktop for as long as that screen is served. This reverses ADR 0056's "exists
only for the one capture" for this one purpose. It does not apply to the UAC
worker, which is untouched.

The worker is the helper's binary, not the desktop app and not the host. The
agent needs a window and an encoder; the logon screen needs neither. The GDI
capture of `Winlogon` already exists once, in `crates/service`. That crate
names no lumepeer dependency (ADR 0049 §2), so **the worker publishes raw
`BGRA8`, not an encoded payload**. See "Not built" for what follows from that.

`logon_screen.rs` sits in `crates/service`'s library next to `agent_launch.rs`,
which already holds `CreateProcessAsUserW` for the agent. Its token-duplication
sequence duplicates the private copy in `secure_desktop_launch.rs`. That is
deliberate and follows ADR 0087 §1: the helper's copy is in its binary, and
merging the two would change a working path that can't be re-verified here.

### 2. A transition is a latched state, entered from the SCM

`LumepeerHost` registers with `RegisterServiceCtrlHandlerExW` and accepts
`SERVICE_ACCEPT_SESSIONCHANGE`. The handler only parses the event
(`session_change::from_wts_event`, platform-independent and tested) and queues
it. The supervision loop and the watchdog drain the queue.

`SessionScreen` gains `ScreenState::Switching`. Any sign-in, sign-out, lock,
unlock, console connect or disconnect, or remote connect or disconnect **in the
served session** does these things:

- ends the attachment;
- clears the frame counter;
- makes `no_picture()` answer `SessionSwitching`;
- leads the host to clear the mapping (`Writer::clear`) before stopping the old
  process.

The state is **latched**. Rereading it changes nothing. It is left only for a
state something confirmed: an agent or worker that attached in the expected
session, or the machine reporting there is no console session. This is the
secure-desktop flag bug (ADR 0056's flicker) avoided by construction rather
than fixed afterwards.

A lock is latched from the notification too, not inferred from a capture that
started failing. While locked, the host serves the logon screen instead of the
agent, because a locked session's desktop *is* `Winlogon`.

The guest's session and grants are untouched. None of this reaches
`SessionManager`.

Something that attaches out of turn is answered with `Shutdown`, not served.
That covers the wrong session, or nothing launched at all.

### 3. Disclosure when nobody is watching

- **A new audit kind.** `AuditEvent::EmptyMachineLogin { accepted }` replaces
  `UnattendedLogin` when `ViewWindows::nobody_signed_in()` is true. Nothing else
  will ever tell the next person at the machine that a guest was let in. A
  *locked* session is not empty, because its owner has an indicator waiting.
- **Signing in during a live session.** The runtime's `set_host_bar` still
  doesn't drive the indicator (ADR 0087 §3). It now records the guest count,
  and an agent attachment that begins while guests are connected is logged as
  an arrival. The indicator is still the first command that attachment gets,
  before `StartCapture`. It has nothing to press, and no event a guest can cause
  lowers it (tested).
- **No banner on the logon screen itself.** The only desktop to draw on is
  `Winlogon`, and a `LocalSystem` process putting up windows there is worse
  than the problem. **The known gap:** somebody who walks up to a logon screen
  while a guest is watching, and types their password, sees no banner until
  they are signed in. Closing that needs its own decision.

### 4. One console session, and nothing else

Changes in any session other than the console's are `SessionAction::Ignore`.
They are logged as "that session is not served" and change no state. RDP
sessions are not served. Serving several sessions at once needs its own ADR.

### 5. No new grant — reversing one sentence of ADR 0085

ADR 0085 said the logon screen "gets its own grant and its own ADR in pack 26".
This ADR **does not add one**, and follows the pack file instead. The logon
screen is `WinSta0\Winlogon`, the same desktop that `secure_desktop` (seeing)
and `secure_desktop_input` (typing) already gate independently (ADR 0056,
0061). A third grant for the same desktop would be two switches with one
meaning, and a guest could have one without the other with no coherent result.

The cost is real. A guest who holds those grants for UAC prompts also holds
them for the logon screen of a machine nobody is signed in to. That is why this
ADR is *proposed* and not accepted.

## Not built, and why

- **The picture does not reach a guest yet.** ADR 0087 left the relay from
  mapping to guest unbuilt, with four named obstacles, and this ADR does not
  build it either. The logon screen adds a fifth: its frames are raw `BGRA8`,
  so somebody has to encode them. The worker can't (ADR 0049 §2). The host
  could, but only by linking a media pipeline into a `LocalSystem` process
  that ADR 0085 §1 keeps free of one. Until that is decided, a guest gets the
  honest `MediaUnavailable` of §18 — for the logon screen as well as for a
  signed-in desktop.
- **`secure_desktop` (view) is not checked for the logon screen**, because
  there is no frame path to put the check on. Whoever builds the relay must
  gate logon-screen frames on it, as `secure_desktop_frame` does for a client.
- **No banner on the logon screen** (§3).

## Verification

Built, with tests that run anywhere:

- `session_change.rs`: every `WTS_*` code over the whole range.
- `SessionScreen`: every transition, the latch, a fast user switch showing
  nothing of the session it left, lock and unlock, ignoring non-console
  sessions, the indicator before capture on a sign-in during a live session, no
  event lowering the indicator, out-of-turn attachments turned away, and the
  empty-machine and secure-desktop answers.
- The seam, the injector's capability, the input gate and the audit kind.

Needs hardware, and **none of it has been run**. This machine's
`LumepeerHelper` must stay as it is, and there is no second machine:

- Guest connects to a machine nobody is signed in to: the worker starts on
  `Winlogon`, the channel attaches, frames land in the mapping.
- Guest types a username and password (with `secure_desktop_input`) and the
  session continues onto the new desktop, indicator first.
- Lock → unlock → fast user switch → sign-out: at every step the guest sees an
  honest state and never a frame of another session.
- The indicator appears when somebody signs in during a live session.
- `EmptyMachineLogin` records appear for admissions with nobody signed in.
- An RDP session's sign-in changes nothing and is logged as not served.
