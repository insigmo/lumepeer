# ADR 0092 — A refused duplication is not proof of a secure desktop

Status: accepted
Date: 2026-09-18

Answers two reports that turn out to share a cause: "in UAC mode I cannot do
anything", and "when I connect to a virtual machine in VMware I cannot control
anything any more". Amends
[ADR 0057](0057-lumepeer-takes-full-local-control.md) and
[ADR 0065](0065-a-chord-is-a-position-on-the-keyboard.md) on one point each.

## Context

Three facts, none of them new, that had never been put together.

**`E_ACCESSDENIED` from `DuplicateOutput` is not a secure desktop.** It is
"this process is not being given the current desktop image", and the secure
desktop is one of several things that produce it. A full-screen exclusive
application — a game, or a VMware Workstation window with a virtual machine in
it — a display-mode change and a graphics-driver reset all refuse duplication
in exactly the same way. `map_start_error` and `map_runtime_error` report all
of it as `MediaError::SecureDesktopActive`, and that is the only name the rest
of the system has for it.

**That name decides where input goes.** ADR 0057 routes a guest's clicks and
keystrokes to the `Winlogon` worker while `secure_desktop_blocked` is latched,
and that flag is set from the capturer's report. So for an episode that was
never a secure desktop, every click and keystroke was handed to a worker whose
`SendInput` could only answer `ERROR_ACCESS_DENIED` — and pointer *moves* were
not even attempted: ADR 0057 caches them rather than spending a `LocalSystem`
process on each one, and that cache is only ever read by a click that goes to
the same worker. Nothing moved, nothing clicked, nothing typed, for as long as
it lasted. That is "I cannot control anything any more", exactly.

**And nothing said which had happened.** The service log on this machine says
the same thing for every episode over four days, nine times out of nine, with
no successful capture ever:

```text
2026-09-17T20:00:02  WARN secure-desktop worker: nothing to capture
2026-09-17T20:00:02  WARN could not capture the secure desktop
```

`capture()` collapses five different failures into one `None`, so "nothing to
capture" named none of: no screen, no device context, no DIB section, a blit
the desktop refused, or a `Winlogon` that was never the desktop receiving
input in the first place.

Separately, and found while reading the same path: ADR 0065's pointer-grab
fallback asks the wrong question. After each absolute move it reads
`GetCursorPos` back and calls the move a miss if the cursor is not where it
was aimed. But `SendInput` is asynchronous — the move is processed by the raw
input thread, and a read issued immediately afterwards can answer with the
position from before it. On a machine where that race goes the same way every
time, and there is nothing to stop it doing so, every move reads back as a
miss, three of them switch the injector to relative motion, and it stays
there: relative motion is subject to the host's own pointer acceleration, so
the cursor drifts away from wherever the operator is pointing and clicks land
somewhere else. The same symptom as the grab the fallback was written to
survive.

## Decision

### A helper that did not land is not the end of the event

In `Network::inject`, when the secure-desktop helper reports that it did not
perform the event, **fall through to the ordinary injector** instead of
returning. The cached pointer position is replayed first, so the click lands
where the operator aimed rather than wherever the host's own cursor sits.

- When the desktop really is secure, that injector answers
  `ERROR_ACCESS_DENIED` in its turn and the event is dropped with a warning,
  which is exactly what happened before.
- When it is not — the VMware case, a game, a mode change — the event lands,
  which is what should have happened all along.

This widens nothing. Reaching that branch at all takes `secure_desktop_input`,
which only a full-control session carries
([ADR 0061](0061-full-control-carries-the-uac-click.md)), and such a session
drives the ordinary desktop through the in-session injector anyway. The
fallback lets an authorized event reach the desktop it was always allowed to
reach; it does not let an unauthorized one reach anything.

### The grab is decided by a cursor that does not move, not by one that is late

`absolute_move_landed` replaces the "is it where it was aimed" test. The
question that distinguishes a pinned pointer from a late read-back is **did
the cursor move at all when it was asked to**:

- Close to the aim: landed, no further argument needed.
- The aim did not move: no evidence either way, and a pointer the operator is
  holding still must not accumulate misses.
- Otherwise: a miss only if the cursor is at exactly the coordinates the last
  read-back gave. A grabbed pointer is pinned and answers the same
  coordinates however far the aim travels; a read-back that is merely late
  answers the *previous* aim, which moves as the operator does.

`GRAB_MISSES_BEFORE_RELATIVE`, `GRAB_PROBE_EVERY` and the `Grab` state machine
are unchanged. What changed is the evidence fed into them.

### The secure-desktop capture says which step refused it

`gdi_snapshot` logs the screen it measured, the Win32 error from
`CreateDCW` and `CreateDIBSection`, and — for the one failure that says
something about the *desktop* rather than about this process — a blit that was
refused, with both sizes and both pointers. The worker adds the fact that
decides whether any of it could have worked: `input_desktop_name()`, which is
`"Winlogon"` for a real secure desktop and `"Default"` for an ordinary one.

`"nothing to capture"` with `input_desktop="Default"` is a full-screen
application or a mode change. With `input_desktop="Winlogon"` it is a genuine
secure-desktop failure. Until now those two were the same line.

## Consequences

- A full-screen VMware window, a game or a display-mode change on the host no
  longer costs the guest its keyboard and mouse. It still costs the picture,
  because `DuplicateOutput` is refused and there is nothing to send — but a
  guest that can still click is a guest that can get out of it.
- A machine whose `GetCursorPos` lags no longer switches itself to relative
  motion for the rest of the session. This is a fix for *every* session on
  such a machine, not only for one with a VMware window in it.
- The next "nothing to capture" says which of five things failed and whether
  `Winlogon` was the input desktop at the time, so the UAC picture can be
  diagnosed from one reproduction instead of guessed at.
- Tests: `a_late_read_back_is_not_a_pointer_somebody_else_is_holding`
  (`crates/media`), covering the pinned pointer, the late read-back, the
  rounding tolerance, a still pointer and the first move of a session.

## Still open

- **The UAC picture is not fixed.** This change says which step refuses it;
  it does not say why, because the answer is not in the code. On this machine
  the GDI capture of `Winlogon` has never once succeeded, and the reason will
  come from the new log lines on the next reproduction, not from more reading.
- The input fallback is not unit-tested. It turns on
  `lumepeer_service::client::inject_secure_desktop` answering `false` and on
  `platform_injector()`, neither of which a test can substitute today; that is
  the same position ADR 0057's injection has always been in, and it belongs in
  `docs/release-checklist.md`.
- The guest is still shown nothing while the host is blocked and has no
  secure-desktop picture to serve. ADR 0063 removed that banner because it
  flickered, and its premise — that the picture underneath *is* the secure
  desktop — is false on exactly this path. Worth revisiting, and not here.
- The relative-motion fallback has still never been confirmed to reach a guest
  OS through VMware's own input path. What this change fixes is the injector
  choosing that fallback when nothing was holding the pointer at all.
