# ADR 0140 — Windows autostart is a logon task, not a Run entry

Status: accepted
Date: 2026-10-02

Corrects the Windows half of ADR 0042 (autostart, cited in
`apps/desktop/src-tauri/src/autostart.rs`) and
[ADR 0103](0103-autostart-is-turned-on-once-by-the-app-itself.md).

## Context

Reported 2026-10-02: a guest reaches the host at the sign-in screen
([ADR 0126](0126-the-logon-screen-is-hosted-by-the-same-host.md)), types the password, and the
connection drops for good. After sign-in the logon host steps down as designed,
but no client is running, although "start with Windows" is on.

The client is `requireAdministrator` ([ADR 0057](0057-lumepeer-takes-full-local-control.md)).
Windows does not elevate `HKCU\...\Run` entries at sign-in; it skips them
silently. So the autostart entry never started anything on Windows, and the
guest's redial ([ADR 0106](0106-a-session-that-dropped-is-dialed-again-at-every-host.md)) had
nobody to reach.

## Decision

On Windows the switch now writes a scheduled task
`\Lumepeer\Autostart-<user>`: a logon trigger and principal for that one
account, `InteractiveToken`, `HighestAvailable`, no time limit, normal priority
(4, not the default 7). The elevated client creates it with
`%SystemRoot%\System32\schtasks.exe` by full path. Off deletes the task.

A `Run` value left by an older release still reads as "on" and is moved into a
task on the next start, then deleted.

Autostart still permits nothing (ADR 0042): it only starts the app.

## Consequences

- After the guest signs the host in, the client starts elevated within seconds,
  takes the host role from the stepping-down logon host and the guest's redial
  lands. There is still a short reconnect: two processes cannot share one
  session.
- Verified: Windows accepts the task definition (elevated `schtasks /Create`,
  "Interactive only", runs as the user). Not yet verified on a real reboot.
