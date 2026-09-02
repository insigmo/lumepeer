# ADR 0056 — Secure-desktop capture crosses the session boundary, and the grant is on for every role

Status: accepted
Date: 2026-09-02

Extends ADR 0049 (the helper's second capability) and amends ADR 0054 (full
control carries every independent grant). Answers the same
`docs/bugs/15-secure-desktop-capture.md` D8 as ADR 0049, because ADR 0049 as
shipped never produced a frame.

## Context

ADR 0049 gave the helper a second capability — capture one frame of the
secure desktop (`Winsta0\Winlogon`) and publish it to a shared mapping — and
wired the whole chain from the grant, through the pipe opcode, to the
`view.rs` encode arm that serves the frame. In practice it never returned a
picture. Two reasons, both confirmed on a real Windows host:

1. **The capture ran in the wrong session.**
   `crates/service/src/secure_desktop.rs::capture` switched the **service's
   own thread** onto `WinSta0\Winlogon`. But the service runs as
   `LocalSystem` in **session 0**, and a window station is a per-session
   named object: `OpenWindowStationW("WinSta0")` from session 0 resolves to
   *session 0's* `WinSta0`, not the console session's, and there is no
   session parameter on `OpenWindowStation` to point it at another one. The
   secure desktop a local administrator authenticates on lives in the console
   session (`\Sessions\1\Windows\WindowStations\WinSta0\Winlogon`), which the
   service's own thread cannot name, let alone switch onto. So the switch
   "succeeded" against the wrong, empty object, or failed outright, and the
   caller always fell through to `docs/bugs/11-uac-degradation.md`'s message.

2. **The GDI capture could not open a device context.**
   `gdi_snapshot` called `CreateDCW(NULL, NULL, NULL, ...)`. With both the
   driver name and the device name null, `CreateDCW` returns `NULL`. It never
   produced a bitmap even when a desktop was reachable.

ADR 0049's own "Verification" section is candid that neither the real
session-0-to-console capture nor `OpenDesktopW("Winlogon")` succeeding for a
`LocalSystem` service was ever exercised — only that the error paths did not
crash against the ordinary desktop. This ADR is what those two unverified
assumptions turned out to require.

## What actually reaches the console session's secure desktop

Only a process **running in the console session, whose thread is on the
`Winlogon` desktop**, can capture or inject there. A session-0 service cannot
put its own thread there. The supported way for a `LocalSystem` service to run
code on the interactive secure desktop is the same technique remote-assistance
services have always used to "show the UAC prompt":

- `WTSGetActiveConsoleSessionId()` names the console session.
- The service duplicates its own `LocalSystem` token, sets the duplicate's
  session id to the console session (`SetTokenInformation(TokenSessionId)`,
  which needs `SeTcbPrivilege` — a privilege `LocalSystem` holds and an
  ordinary user does not), and
- `CreateProcessAsUserW` launches a **worker process** with
  `STARTUPINFOW.lpDesktop = "WinSta0\\Winlogon"`, so the worker starts in the
  console session already attached to the secure desktop.

The worker is this same `lumepeer-service.exe`, re-executed with one
argument, `--secure-desktop-worker`. It opens the shared mapping the service
already created, takes one GDI snapshot of the desktop it is on, writes the
frame, and exits. Its exit code is the whole answer the service reports back
over the pipe: `0` means a frame is in the mapping, anything else is
`STATUS_REFUSED`.

## Decisions

### 1. The service launches a short-lived worker; it does not capture in-process

`serve_secure_desktop_capture` (`windows_service.rs`) keeps ADR 0049's session
check unchanged, then:

1. Ensures the shared mapping exists by holding a single
   `frame::Writer` for the service's lifetime (created lazily on the first
   capture request, kept alive so the client can read the frame after the pipe
   reply — the ordering ADR 0049 §2 already depends on).
2. Launches the worker into the console session's `Winlogon` desktop.
3. Waits for it, bounded by `SECURE_DESKTOP_WORKER_TIMEOUT_MS`, and reads its
   exit code.

The worker, not the service thread, is what ever touches `Winlogon`. This is
strictly narrower in standing surface than a persistent capturer would be: a
`LocalSystem` process on the interactive secure desktop exists only for the
few milliseconds one capture takes, then is gone. Nothing about the pipe
protocol changes — still two bytes in, two bytes out, one fixed opcode with
no parameters. The worker is spawned by the service from a path the service
already trusts (its own `current_exe`), with a fixed argument the peer cannot
influence; the client cannot ask for "capture desktop X" any more than it
could before.

### 2. The worker writes the mapping the service created

`frame::Writer` gains `open()` alongside `create()`: the service `create()`s
the `Global\` mapping (which needs `SeCreateGlobalPrivilege`, held by
`LocalSystem`), and the worker — also `LocalSystem`, in the console session —
`open()`s the existing one for write. The mapping's DACL already admits
`SY` for full access and `IU` for read only (ADR 0049), so nothing about who
can read a frame changes: only `LocalSystem` (the service and its worker) can
write one, exactly as before, and an interactive user still only reads.

### 3. The GDI capture opens a real device context

`gdi_snapshot` opens the desktop DC with `CreateDCW("DISPLAY", NULL, NULL,
...)` — the documented way to get a DC for the display the calling thread's
desktop is on — instead of the all-null call that always returned `NULL`.
This is the same whole-screen capture `crates/media`'s Windows backend does
for its own first frame, kept reimplemented here rather than shared for
ADR 0043's dependency-minimalism reason (ADR 0049 §2).

### 4. The grant is on for every role, and stays independently revocable

ADR 0049 made `secure_desktop` deny-by-default and derivable from no role,
reasoning that "a guest asking to watch an administrator authenticate is a
different decision from a guest asking to control the mouse". ADR 0054 then
made every *other* independent grant ride `FullControl` while leaving this one
off. The result was that a plain view-only guest — the exact case the user hit,
watching a remote screen when a UAC prompt fired — could never be shown the
prompt at all, because no role granted it and ADR 0054 had removed the host
panel's per-grant switches that used to turn it on by hand.

**`Grants::from_role` now sets `secure_desktop: true` for every role**, the
same way `view` is on for every role. Seeing that the remote machine is asking
for administrator consent is treated as part of *watching the screen*, not as
a capability above it: the honest degradation of
`docs/bugs/11-uac-degradation.md` already tells every guest "a secure prompt
is showing" — this only replaces that message with the picture it is
describing, for the guest who is already permitted to see the rest of the
screen.

The flag **stays an `IndependentGrant`**: `IndependentGrant::SecureDesktop`,
`Grants::get`/`set` and `session_set_grant` are untouched, both arms still
exhaustive with no `_`. A host can still withdraw it from a running session,
and the actor still re-reads the live grant before every secure-desktop
capture (`view.rs`, unchanged since ADR 0049), so a revoke takes effect on the
next poll. What changes is only where a session *starts*: on, not off.

This narrows what ADR 0049 §Decision 1 and ADR 0054's grant table said; those
two documents' reasoning is superseded on this one point by the plainer fact
that a message reading "the remote machine is asking for administrator
consent, respond there" already discloses that a secure prompt is up — the
picture of it is not a categorically new disclosure to a guest who can see the
desktop the prompt appears over.

## What this ADR does not yet do

Showing the guest the secure desktop is viewing only. **Answering** the prompt
from the guest side — typing the administrator password and clicking through —
is input injection onto the `Winlogon` desktop, which is a second, larger
capability on the same worker (it is the only place a `SendInput` can reach
that desktop). That is deliberately not built here: this ADR is the capture
half, verified first, and the input half extends this same worker under its
own decision recorded when it is built. Until then a guest sees the prompt and
answers it at the machine, which is `docs/bugs/15`'s own baseline behaviour
made real.

## Consequences

- `crates/service` gains a worker entry point (`--secure-desktop-worker`) and
  a token-duplication / `CreateProcessAsUserW` launcher. Both are new `unsafe`
  Win32 surface in a crate that already carries `unsafe` for the SCM, the
  DACL'd pipe and the mapping, under ADR 0012's justification standard.
- The service now spawns a child process per capture request. It is bounded
  (`SECURE_DESKTOP_WORKER_TIMEOUT_MS`), from a fixed path with a fixed
  argument, and short-lived. A capture that hangs is killed and reported as
  refused rather than blocking the single-threaded accept loop indefinitely.
- Every failure mode still lands on `docs/bugs/11-uac-degradation.md`'s
  honest message: service absent, grant off, session check refused, the
  worker failing to spawn, exit non-zero, or the mapping unreadable. The new
  path only ever *adds* an attempt before that unchanged fallback runs.
- ADR 0054's claim that a host can withdraw `secure_desktop` from a running
  session still holds at the actor level, but the panel no longer surfaces a
  switch for it (ADR 0054 removed all of them). Restoring a per-session
  switch, if a host wants to deny a view-only guest the prompt, is a UI
  follow-up, not a change to the grant model above.

## Verification

- `Grants`/`IndependentGrant`/`SessionManager` (`crates/core`): the existing
  generic property and unit tests over `ALL_INDEPENDENT` still hold — the
  grant is independent, revoked immediately, cleared on revoke — with the
  role-default tests updated to expect it on for every role.
- `frame::Writer::open` round-trips against a mapping `create`d in the same
  process (unelevated, so the `Global\` create is expected to fail on this
  machine's own test run exactly as ADR 0049's `frame` tests already note).
- The worker launch and a real capture were exercised end to end on a real
  Windows host by rebuilding and reinstalling the service and driving the
  pipe: the worker lands on the console session's `WinSta0\Winlogon` and the
  GDI capture returns a correctly sized BGRA frame. Driving a *live* UAC
  prompt through it to a connected guest remains a manual step (ADR 0049's
  own instruction not to trigger a real secure-desktop transition in an
  automated test), now with a mechanism that actually reaches the object.
