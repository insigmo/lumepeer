# ADR 0102 — The Windows shell is started by an unelevated worker

Status: accepted
Date: 2026-09-22

Amends [ADR 0079](0079-a-terminal-is-its-own-grant-and-never-the-clients-privileges.md),
whose decision 2 — the shell runs as the person sitting at the host, never as
the client — is unchanged. What changes is the mechanism on Windows, because
the one ADR 0079 named cannot be used by the client ADR 0057 ships.

## Context

ADR 0079 decision 2 is implemented on Windows by taking the token of the
process behind `GetShellWindow()` — Explorer, running unelevated as the
interactive user — duplicating it as a primary token, and starting
`%COMSPEC%` with it under a `ConPTY`. `CreateProcessAsUserW` was the call,
because it takes a `STARTUPINFOEXW` and a `ConPTY` child is handed its console
through a process-thread attribute list.

[ADR 0057](0057-lumepeer-takes-full-local-control.md) then
shipped the client with `requireAdministrator`. Every remote terminal on every
Windows host has been refused since, with `CannotDropPrivileges` — the honest
answer to a spawn that failed, and one nobody could act on because the reason
never left the host.

It is a privilege problem, and it was measured rather than reasoned about
(`docs/bugs/18-terminal-close-and-shell-spawn.md` has the run). On an elevated
administrator token, with Explorer present and its token duplicated
successfully:

| | unelevated | elevated |
| --- | --- | --- |
| `CreateProcessAsUserW` | works | `ERROR_PRIVILEGE_NOT_HELD` (1314) |
| enable `SE_INCREASE_QUOTA_NAME` | not in the token | enabled, and still 1314 |
| enable `SE_ASSIGNPRIMARYTOKEN_NAME` | not in the token | **not in the token** |
| `CreateProcessWithTokenW` | — | works |

`CreateProcessAsUserW` wants `SE_INCREASE_QUOTA_NAME`, which an elevated token
has but disabled, and `SE_ASSIGNPRIMARYTOKEN_NAME` for a token that is not the
caller's own — which an elevated administrator token does not contain at all
and therefore cannot enable. Turning on the one that is present does not help.
There is no combination of flags that makes this call available to this client.

`CreateProcessWithTokenW` wants only `SE_IMPERSONATE_NAME`, which an elevated
client does hold, and the process it starts is the interactive user at medium
integrity — exactly the drop ADR 0079 asks for. It ignores `lpAttributeList`,
so it cannot start a `ConPTY` child.

## Decision

### 1. The client starts a worker; the worker owns the pseudo-console

`crates/terminal-worker` is a small binary that creates the `ConPTY` and
starts `%COMSPEC%` under it with a plain `CreateProcessW`. It needs no token
and juggles none: the client started it *as* the interactive user, so it
already is the identity the shell should have.

The client keeps doing the part that is a decision — finding the interactive
user and refusing when it cannot — and stops doing the part that is plumbing.

### 2. The console is in the worker and nowhere else

There is no second implementation kept for the unelevated case. A path that
only ever runs on a developer's machine is a path that is only ever tested
there, and this one had been broken in two separate ways that no test noticed,
because the only configuration that mattered never reached it at all
(`docs/bugs/18`). One implementation is taken by a `cargo test` run and by an
installed client alike.

### 3. Unelevated, the worker is started plainly — but only when that is the
same thing

An unelevated process holds no `SE_IMPERSONATE_NAME` either, so it cannot use
the token. Starting the worker as ourselves is the same answer **only** when
this process is already the interactive user and is not elevated. Both halves
are checked, against the token, before it is done:

- the same user elevated is the administrator shell this crate exists to
  prevent;
- a different user unelevated is a shell belonging to somebody who is not at
  the machine.

Anything else is `CannotDropPrivileges` with no process created. This is the
Unix rule of ADR 0079 — "the client already *is* that person, unless it is
root" — written out for Windows, and it is checked rather than assumed.

### 4. Two pipes, framed one way

The worker's standard input and output are two anonymous pipes the client
holds the other ends of. Handles do cross `CreateProcessWithTokenW`, which is
what makes this possible without a named pipe and the access control one would
need; the ends the client keeps are taken back out of inheritance immediately,
so no later child of the client gets a handle onto somebody's terminal.

Back from the worker comes what the shell said and nothing else, so it is
unframed. Towards it travel two things — keystrokes and a new geometry, since
`ResizePseudoConsole` is now a call only the worker can make — so that
direction carries a one-byte kind and a length.
`lumepeer_terminal::worker_protocol` holds both sides of it.

### 5. The shell dies with the worker, and the worker dies with the session

ADR 0079's orphan rule has to survive an extra process. The worker puts the
shell in a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the shell
goes when the worker's handles close — including when the worker is
`TerminateProcess`d and runs no destructor of its own. The client terminates
the worker from the same `Drop` that used to terminate the shell, so a session
that ends by its connection dropping still leaves nothing behind.

### 6. The worker says when there is a shell

It writes one byte once the shell is running and adopted. Until that byte
arrives the client reports no shell at all, so a worker that could not start
one becomes a refusal the person can read rather than a terminal that opens
and closes again. Same arrangement as the decoder worker's readiness byte
(§11.3).

## Consequences

**This does not widen what a guest may do.** The decision was made before this
process exists: `terminal_allows(&peer)` on the actor loop, re-read for every
shell. The worker holds nothing its own user does not already hold — run it by
hand and you get your own shell, which the Start menu also offers — so it is
not a privileged helper and there is nothing in it to escalate through. The
same argument as `crates/host/src/input.rs`: the privileged side performs, it
does not decide.

**It is a third sidecar.** `lumepeer-terminal-worker` ships through Tauri's
`externalBin` beside `lumepeer-decoder-worker` and `lumepeer-service`, and is
staged for every target because `externalBin` is not per-platform. Off Windows
it exits with an explanation, like the service does: Unix needs no drop,
because the client there already is the interactive user. A Windows client
whose worker was never staged refuses the shell — it does not start one some
other way.

**The service is not involved.** The other way to get
`SE_ASSIGNPRIMARYTOKEN_NAME` is `LocalSystem`, and `crates/service` already
starts processes for the interactive session. It was not used: it would make
the terminal depend on a service being installed, and it would put a shell
launch behind the privileged helper of ADR 0043, which today can express one
operation and no strings at all. The client already holds everything this
needs.

**Nothing changes for Unix.** `crates/terminal/src/unix.rs` is untouched: a
client that is the interactive user starts the shell itself, and a client
running as `root` is still refused.
