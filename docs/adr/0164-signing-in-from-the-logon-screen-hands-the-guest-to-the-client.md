# ADR 0164 — Signing in from the logon screen hands the guest to the client

Status: accepted
Date: 2026-10-20

Amends [ADR 0126](0126-the-logon-screen-is-hosted-by-the-same-host.md) §5 and
closes its "A seamless sign-in" and "Starting the client after sign-in" gaps
for a sign-in a guest makes.

The owner's report: after a reboot the guest reaches the host at the sign-in
screen and signs in, and then the connection is gone — lumepeer is not running.

## Context

On `beta`, 2026-10-20, both logs and the machine say the same thing:

- 07:16:41 the guest connects to the logon host and is admitted with the
  device password. 07:16:48 the guest signs in; the service stops the logon
  host ("somebody signed in"), which leaves without a word — the endpoint is
  dropped, not closed.
- The guest notices only at the 8-second idle timeout (07:16:56), then tries
  to resume a session the next process will never have heard of. At 07:17:08
  the owner gave up and closed the view window.
- Nothing hosted again until the owner started the client by hand at 07:17:37.
  `beta` has no `\Lumepeer\Autostart-bberb` task and no `Run` entry: autostart
  is off there, and ADR 0140's task is the only thing that would have started
  the client.

So the hand-over ADR 0126 §5 describes has two holes. The client starting is
left to a separate, optional switch — off is a legitimate choice (ADR 0042),
and with it off a guest who signs the machine in loses it. And the guest is
not told the logon host is going, so it spends the gap on a dead link and a
resume that cannot succeed.

## Decision

### 1. The logon host says it is going, and whether it had a guest

When asked to give the role up, the logon host closes every connection with
`CLOSE_RESTARTING` — the close ADR 0148 gave an updating host, which means the
same thing here: this process is going and the next one has never heard of
the session. A guest reading it skips the idle timeout and the resume, keeps
its view window up, and dials a new session after two seconds, knocking until
the client answers.

It then exits with `LOGON_HOST_EXIT_HANDED_OVER` (5) if any guest was
connected or being admitted, and 0 otherwise.

### 2. The service starts the owner's client when the logon host had a guest

When the service stopped the logon host because somebody signed in (or the
console moved), and it exited `HANDED_OVER`, the service starts
`lumepeer-desktop.exe` — the one beside it, the binary the logon host ran — in
the console session (`crates/service/src/client_launch.rs`):

- **Only for the owner.** The signed-in user's SID must be the logon host's
  owner (`users\logon-host-owner`): the guest's redial is for that identity,
  and another account's client would be a different host.
- **Only if nothing hosts yet.** If the host role is already held — the
  autostart task was quicker — nothing is started.
- **The way the autostart task would.** The full half of the user's split UAC
  token (`TokenLinkedToken`), which is what `HighestAvailable` gives the task
  (ADR 0140); the user's own environment block; `WinSta0\Default`; no
  arguments. A standard user's token is used as it is, and the
  `requireAdministrator` client is then refused exactly as that user's own
  task would be.

A sign-in with no guest on the logon host starts nothing: that is autostart's
business, and with autostart off, off stays off. A guest who signed the
machine in is the opposite case — the client is the continuation of a session
already running, not a new one.

## Consequences

- After a guest signs the host in, the client is started within about a second
  of the logon host leaving, whether or not autostart is on, and the guest's
  redial lands in the view window it never closed. There is still a short
  reconnect: two processes cannot share one session.
- With autostart on as well, the task and the service may both start a client;
  the single-instance lock turns the second away, which at worst brings the
  first one's window forward.
- A logon host that is terminated after the five-second stop timeout reports
  nothing, so no client is started for it; the guest's dial waits for
  autostart, as before.

## Not done

- **Linux and macOS** (ADR 0151). Their sign-in-screen host is stopped by
  SIGTERM and does not announce anything yet, and the supervisor starts no
  client. Autostart there is a per-user entry that needs no elevation and is
  on by default, so the missing client is the narrower case.

## Verification

Unit tests: the supervisor's `stopped_with`, the launcher's `verdict`, the
logon host's `exit_code_for`.

Needs a real sign-in — `docs/release-checklist.md`, "Hosting the sign-in
screen".
