# ADR 0133 — A locked host is still the host

Status: accepted
Date: 2026-09-30

The user's report: Lumepeer has to keep working when the host's screen is
locked, at the Windows sign-in screen, and everywhere else, and must never
throw the guest out over such a moment. The symptom they saw was a picture
and input that died.

Amends [ADR 0126](0126-the-logon-screen-is-hosted-by-the-same-host.md) §3
on one point, the default.

## Context

Reproduced on 2026-09-29 between this machine (guest) and `beta` (host,
v0.0.113). `beta` was locked with `LockWorkStation` in the middle of a
full-control session.

- **While locked, the picture worked.** The helper published a 1920×1080
  frame of the lock screen every ~0.5 s (ADR 0056), and the guest saw it.
- **Then an ordinary drop.** `beta` loses its direct path every few minutes
  with or without a lock (19:40, 19:52 and 20:14 that evening). The session
  resumed 21 s later, inside its window (ADR 0089).
- **The resume found the screen locked.** `start_granted_session` asked the
  capturer to start. `WindowsCapturer::start` returned
  `SecureDesktopActive`, because `DuplicateOutput` refuses a locked desktop.
  The actor logged "consent granted but this platform cannot capture" and did
  not register the viewer.
- **From then on the guest had no picture at all.** The guest dialed a media
  stream about once a second, and every encode loop ended at once on "no
  viewer holds a view grant: capture must not run". That lasted to the end of
  the session, unlocked or not.

The capturer already knew how to ride out a secure desktop for a viewer who
was *already* watching: `next_frame` drops the duplication and retries it on
a backoff for as long as the lock lasts. Only a viewer who arrived *during*
the episode was refused: a guest resuming after a drop, a guest dialing a
locked machine, or a monitor switch.

Two further gaps in the same space:

- **Sleep.** Nothing on the host held off idle sleep during a session. A host
  whose power plan sleeps after N minutes fell asleep under a guest who was
  only watching. On a lock screen that includes a guest who was moving the
  pointer, because those moves are cached rather than sent (ADR 0057).
- **The sign-in screen.** ADR 0126 hosts it only after a separate checkbox is
  ticked, and it is off by default. The report asks for it to work.

## Decision

### 1. Starting behind the secure desktop is waiting, not refusing

`WindowsCapturer::start` treats `SecureDesktopActive` from opening the
duplication as a start that is already inside the reopen loop:

- the target is kept;
- a recovery is due at once;
- `start` returns `Ok`.

The viewer is registered. The encode loop's secure-desktop arm serves the
helper's frames of the lock screen until the lock clears, then ordinary
capture resumes, exactly as for a viewer who was watching when the screen
locked.

Every other failure still refuses. A monitor that does not exist is the
caller's mistake, not an episode to wait out.

### 2. A host with a guest does not sleep for idleness

While the actor hosts anybody, it holds a Windows power request of type
`PowerRequestSystemRequired` (`lumepeer_service::stay_awake`). "Anybody"
includes a session inside its reconnect window: a host that fell asleep
during a drop would turn a resumable blip into the end of the session.

- **Only idle sleep is stopped.** The display still turns off, and the screen
  still locks, on the owner's schedule.
- **It is visible.** `powercfg /requests` names Lumepeer and the reason.
- **Why a request and not `SetThreadExecutionState`.** A request is a handle,
  not a property of a thread, so it holds whichever worker thread the actor
  is on.
- **The logon host runs the same actor**, so it holds the same request.

A refused request is logged once and not asked again until the next session.

### 3. The sign-in screen is hosted whenever a device password is set

This reverses ADR 0126 §3's "off by default".

- **When it turns on.** Setting a device password, and every client start
  with one already set, write the account's SID to `logon-host-owner`.
  The start case is what hosts machines whose password predates this ADR.
- **What the owner file cannot say.** The file says whose store to open. It
  cannot say "the owner said no".
- **So the switch records the refusal.** Turning the switch off writes
  `logon-host-declined` into the account's own protected directory, and
  turning it on again removes it. A declined account is never hosted
  automatically.
- **One owner per machine** (ADR 0126 §3) still holds. The automatic path
  never moves another account's ownership; only the switch does.

Nothing else about the logon host changes. It still refuses without an
identity or a device password. It still admits only a guest with the password,
and still leaves at sign-in.

### 4. The secure-desktop input worker says when it performed an event

Until now only failures were logged, so a guest who reported that their keys
did nothing left no trace to tell "never arrived" from "arrived and did
nothing". The worker now logs one line per event performed. It does not say
which key: on a lock screen or the sign-in screen, the keys are somebody's
password.

## Not done

- **Other platforms.** macOS and Linux get no sleep prevention from this ADR.
  The report and the reproduction are Windows.
- **Lock-screen frame rate.** The picture of the lock screen is still one
  helper process per frame, throttled to 2 frames a second (ADR 0049).
- **Autostart after a remote sign-in.** A guest who signs the machine in from
  the logon host depends on the elevated client starting from the `Run` key
  (ADR 0126, "Not done"). `beta`'s Shell-Core log shows it doing so within a
  second on 2026-09-28. Why that works with UAC at its default is not
  explained.

## Verification

Unit tests:

- `a_start_behind_the_secure_desktop_waits_instead_of_refusing` (media);
- `a_power_request_is_held_and_released` (service);
- `a_declined_logon_screen_is_remembered_until_taken_back` (service).

Live on `beta`: see `docs/release-checklist.md`, "Locked and signed-out
hosts".
