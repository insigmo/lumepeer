# ADR 0079 — A terminal is its own grant, and never the client's privileges

Status: accepted
Date: 2026-09-10

Follows [ADR 0078](0078-a-tunnel-is-a-grant-plus-an-address-the-host-named.md)
in shape — a capability nothing already granted implies gets a flag of its own,
a lazily opened ALPN of its own, and a block of appended messages behind a
feature string — and answers the one thing ADR 0078 did not have to:
[ADR 0057](0057-lumepeer-takes-full-local-control.md) makes the Windows client
run `requireAdministrator`, so a shell spawned as a child of this process would
be an administrator shell by inheritance rather than by anybody's decision.

## Context

Backlog item 22 is a remote terminal. Measured by what a guest can do with it,
it sits above `input`: an operator watching the screen can see a guest type
`rm -rf`, and a terminal is a channel with no picture attached to it. It is
also the first capability in this app that *creates a process* — every other
one acts on processes that already exist.

Two things therefore have to be decided before any bytes move, and neither of
them is "how does a PTY work".

**What is being permitted.** Not "the guest may run commands": that is what a
terminal *is*, not what the host agreed to. The host agreed that this guest, in
this session, may start a shell — and can take that back mid-command.

**Whose privileges the shell gets.** ADR 0057 states its own cost plainly: the
client runs at high integrity so that a guest can operate elevated windows and
answer UAC. Nothing in that decision says a *shell* should start elevated, and
a shell that did would hand a guest an administrator prompt-free command line
on the strength of a decision made about mouse clicks. Inheriting a token is
not a permission; it is the absence of one.

## Decision

### 1. `terminal` is the tenth independent grant

`Grants::terminal` / `IndependentGrant::Terminal`, carried by
`Role::FullControl` alone (ADR 0054) and separately revocable, like every
other flag on that list. `input` does not imply it and it does not imply
`input`: they are different powers over the same machine, and a host that
handed over the keyboard has not thereby agreed to a shell it cannot see.

**Re-read at every open.** `SessionManager::terminal_allows(peer)` is asked
once per `TerminalOpenRequest`, not when `rd/term/1` came up — the same
per-request rule ADR 0078 gives for a tunnel, and for the same reason: a
session opens many shells over its life, and a revoke that landed only on the
first would be a revoke waiting for somebody to type `exit`. Withdrawing the
grant kills every running shell on the spot and drops the connection.

### 2. The shell runs as the interactive user, or it does not run

The rule is one sentence: **the shell gets the desktop user's privileges, never
this process's.** How that is honoured differs by platform, and where it cannot
be honoured the terminal refuses instead of falling back.

- **Windows.** The client is elevated (ADR 0057), so the drop is explicit.
  `lumepeer-terminal` takes the token of the process that owns
  `GetShellWindow()` — Explorer, which runs unelevated as the interactive user
  — duplicates it as a primary token and spawns the shell with
  `CreateProcessAsUserW`. `CreateProcessWithTokenW` is not usable here: it
  ignores `lpAttributeList`, and a ConPTY child *is* an attribute list. If
  there is no shell window, if the token cannot be opened or duplicated, or if
  the spawn is refused, the terminal answers `TerminalRefusal::
  CannotDropPrivileges` and no process is created. An administrator shell by
  accident is the one outcome this path may not produce (§18).
- **Unix.** The client already runs as the interactive user, so there is
  nothing to drop and nothing to check — except the one case where the
  statement would be false: a client running as `root` refuses with the same
  `CannotDropPrivileges`, because "drop to whom" is a decision nobody made and
  guessing at it would be worse than saying no.

**The guest never names the program.** The shell is `$SHELL`, or `%COMSPEC%` on
Windows, with a fixed fallback (`/bin/sh`, `cmd.exe`) and no arguments. Letting
a guest choose the executable would make every other decision here decorative:
"may start a shell" and "may start any program with any arguments" are not the
same permission, and only the first one was granted.

### 3. A terminal is never silent on the host's screen

While a shell is running, the host sees it — on the session's own row in the
main window, and on the always-on-top session bar of
[ADR 0055](0055-an-always-on-top-session-bar-for-the-host.md), which is the
surface that stays visible while the operator is working in something else.
Neither can be switched off while a terminal is open, exactly as the recording
dot and the secure-desktop dot cannot, and in the same spirit as the unattended
banner of [ADR 0033](0033-unattended-admission-and-keystore-secret-slots.md).
`SessionStatus::terminal_active` is what both hang off, and it is a fact about
running shells rather than about the grant — permission is not the thing worth
interrupting somebody for.

### 4. A fifth ALPN, and four messages at `PROTOCOL_MINOR` 16

`rd/term/1`, `Channel::Terminal`, opened lazily by the side that dialed the
control connection (ADR 0026) and only once a shell has actually been agreed —
the ADR 0032 shape, for the ADR 0078 reason: a channel that is busy must not be
able to delay a revoke on another one, and a shell producing output is busy.
Not a second use of `rd/tunnel/1`, which would put a terminal behind the tunnel
grant and make each one the other's congestion.

The keystrokes and the output ride that connection framed
`u32_be session_id || u32_be len || bytes`, the self-describing header
`rd/tunnel/1` and the file chunks already use, so several shells share one
connection without a second framing layer and a length is refused against
`TERMINAL_OUTPUT_MAX_BYTES` before anything allocates it (§9.1). A `len` of
zero ends that shell's stream.

The control channel carries four messages, appended behind `FEATURE_TERMINAL`:
`TerminalOpenRequest { cols, rows }`, `TerminalOpenResponse { session_id,
refused }`, `TerminalResize { session_id, cols, rows }` and `TerminalClose
{ session_id }`. Two details are worth writing down:

- **The host names the shell, and the guest matches by order.** The request
  carries no id, because the id is the host's answer and not the guest's ask —
  and the control channel is one ordered stream with a strict `seq`, so a guest
  with two opens in flight pairs the responses to the requests in the order it
  sent them. This is `gap-tasks/17`'s own field list; the alternative
  (guest-chosen ids, as `TunnelOpenRequest` uses) would have the guest naming
  something the host owns.
- **`refused: Option<TerminalRefusal>` replaces the `accepted` +`reason` pair**
  the task file names, following what ADR 0078 actually shipped for
  `TunnelOpenResponse`: two fields can disagree with each other, one cannot.
  `TerminalRefusal` is a closed set — `NotGranted`, `TooMany`,
  `CannotDropPrivileges`, `Unavailable`.

### 5. Bounds, all constants

`MAX_TERMINALS_PER_SESSION` (4) is how many shells one session may hold at
once; `TERMINAL_OUTPUT_MAX_BYTES` (64 KiB) bounds one output frame;
`TERMINAL_SCROLLBACK_BYTES` (16 frames' worth) is how much a shell's output may
pile up on the **guest** side for a window that has stopped polling, oldest
first out, which is what a terminal scrolling off the top does anyway — and it
is guest-side because the host keeps no transcript to bound;
`TERMINAL_COLS_MAX` (500) and `TERMINAL_ROWS_MAX` (300) bound a geometry
that arrives from the network and becomes a PTY size — zero or past the bound
is malformed at the parse boundary, not a refusal further down (§9.1).

### 6. The emulator is `xterm.js`, and the command line reaches no disk

Writing an ANSI emulator is not this project's work, and a half-written one is
a rendering bug that looks like a remote-execution bug. The guest side is
`xterm.js` in the view window.

Opening a terminal, closing one and why it closed are audited. **What was typed
and what came back are not** — not to the audit log, not to `tracing`, not to
any file. §15's log is deliberately pseudonymous and deliberately exportable,
and a transcript of somebody's session is the one payload that would make it
neither (ADR 0041). It is the same rule that keeps clipboard contents and file
names out.

### 7. A crate of its own: `crates/terminal`

The code that spawns processes and drops tokens does not belong in
`lumepeer-media`, which is about pixels, and does not belong in
`apps/desktop/src-tauri`, which carries no `unsafe` at all by policy —
`GetShellWindow`/`DuplicateTokenEx`/`CreateProcessAsUserW` and the ConPTY
handles have no safe bindings, and the app crate delegates every such need to a
compiled crate of its own (`fs4`, `keyring`, `lumepeer-service`). It also means
`cargo test -p lumepeer-terminal` runs the orphan-process test without the
Tauri build script, the `requireAdministrator` manifest or the sidecar
binaries in the way.

`portable-pty` carries the Unix side (`openpty`, the controlling terminal,
`waitpid`), under `[target.'cfg(unix)'.dependencies]` so its `winapi`/`winreg`
tree never enters a Windows build. It cannot carry the Windows side: its ConPTY
backend calls `CreateProcessW`, which has no token parameter, so decision 2
would be impossible through it. Windows gets its own ConPTY module over the
`windows` crate already resolved for `lumepeer-service`.

## Consequences

- A guest with full control gets a shell as the person sitting at the machine,
  with that person's rights and no more — on a Windows host whose client is
  running as administrator for every other purpose.
- A Windows host where the drop cannot be made (no Explorer, a locked-down
  policy) gets no terminal at all, and is told which of the two it is. That is
  the intended failure: the fallback would be an administrator shell.
- A terminal cannot be opened quietly. The cost is one more indicator the host
  did not ask for; the alternative is a capability whose whole risk is that
  nobody notices it.
- The session-0 case — a terminal before anyone has logged in — is explicitly
  *not* here. There is no interactive user to be, which makes decision 2
  unanswerable; it belongs to the `25`/`26` batches, which are about session 0
  as a whole.
- Nothing here is SSH. There is no key exchange of its own, no port, no
  listener and no way in that does not begin with a granted Lumepeer session.

## Alternatives considered

- **Let the terminal inherit the client's token.** The `requireAdministrator`
  of ADR 0057 makes this a silent privilege escalation for every Windows host.
  Rejected: ADR 0057 bought elevated-window *input*, and reading it as "and
  therefore an administrator shell" is exactly the "grant implied by another
  grant" this project's ground rules forbid.
- **A `terminal` implied by `input`.** Both are "the guest drives the machine",
  and that is the argument against it: a host granting a keyboard is granting
  something it can watch happen on its own screen. A shell is not on the
  screen.
- **Let the guest choose the shell and its arguments.** It turns the grant into
  arbitrary process execution under another name, and there is no version of
  the host's consent screen that could state what it covers.
- **Run the shell through the privileged helper (ADR 0043).** The helper runs
  as `LocalSystem`, which is the wrong direction entirely — it would have to
  drop *further*, and the ConPTY handles would then have to cross a process
  boundary to get back. The client already holds a token of the right user; it
  only has to stop being elevated.
- **Reuse `rd/tunnel/1`.** It would put a terminal behind the tunnel grant and
  make a 64 KiB `cat` compete with a database session for the same stream.
- **Write the emulator.** Rejected on sight: see decision 6.
