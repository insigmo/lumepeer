# ADR 0084 — Restarting the host is its own grant, and a warned act

Status: accepted
Date: 2026-09-15

Follows [ADR 0079](0079-a-terminal-is-its-own-grant-and-never-the-clients-privileges.md)
in shape — a capability nothing already granted implies gets a flag of its own
and an appended message behind a feature string — and answers the one thing no
previous capability had to: what this project does when a guest asks for
something the host cannot take back afterwards.

## Context

Backlog items 12 and 13 are "survive a restart" and "restart the host". They
arrive together because neither is worth anything alone: a reboot button that
leaves the guest staring at a dead address is a way to lose a machine, and an
automatic reconnect with nothing to reconnect *after* is a solution to a
problem nobody has.

Three things about the existing design decide most of this before any new code
is written.

**The resume window is about networks, not machines.**
`RECONNECT_WINDOW_SECS` is 60 seconds, and what it buys is a resume: the same
`NodeId`, the same `session_id`, **the same grants**
(`crates/net/src/reconnect.rs`). Its length is load-bearing precisely because
grants ride on it. A restart takes longer than a minute on every machine worth
supporting, and the obvious "fix" — stretch the window to cover a reboot — is
the one change that must not be made. It would mean a set of grants outliving
the machine that agreed to them.

**Unattended access already solves coming back.** ADR 0033 and
[ADR 0063](0063-the-host-is-asked-even-when-the-device-can-let-itself-in.md) give a
trusted device a way in when nobody is at the host, and
[ADR 0062](0062-an-invite-code-that-does-not-change.md) keeps the invite code
stable across restarts. The guest side of a reboot therefore needs no new
authentication of any kind. It needs to *wait*, and then do the ordinary thing.

**A restart is not like the other capabilities.** Every grant to date is
revocable in the sense that matters: withdraw it and the thing stops. Clipboard
reads stop, tunnels close, shells die. A machine that has begun shutting down
stops for nobody. It is also the first capability whose cost falls on somebody
who is not a party to the session — the person sitting in front of the host,
who did not ask for any of this and may be mid-sentence in an unsaved document.

## Decision

### 1. `reboot` is the eleventh independent grant

`Grants::reboot` / `IndependentGrant::Reboot`, carried by `Role::FullControl`
alone (ADR 0054) and separately revocable, like every other flag on that list.

It follows `FullControl` by default for the same reason `terminal` and `tunnel`
do — a role that means "this guest operates this machine" is the only role for
which restarting it is in scope at all — and it is separately revocable for the
reason this list exists: a host that hands over the keyboard has not thereby
agreed to lose the machine, and must be able to say so without narrowing the
role and taking the keyboard back too.

### 2. One message, no answer

`MessageKind::RebootRequest { mode }`, appended after `TerminalClose`, behind
`FEATURE_REBOOT` and at `PROTOCOL_MINOR` 17.

There is no `RebootAck`, and this is not an omission. A host that accepts the
request is a host that is about to stop being reachable, so anything it
promised would be a promise it is in no position to keep. A guest learns it was
refused the same way it learns anything else about a machine that is still
there: the machine is still there.

`mode` is `Reboot` or `Shutdown` and the set is closed at two. A third mode
byte is a peer naming something this build has never heard of, and is refused
at the parse boundary rather than guessed at.

There is **no delay field and no force flag**. How long the person at the host
gets is that host's own `REBOOT_WARNING_SECS`, not a number a guest may name —
a guest that could name it could name zero, which is the warning removed by
protocol.

### 3. The host warns, then decides again

A granted request does not restart anything. It raises a banner on the host's
own screen, naming the guest and which of the two acts was asked for, and
raising this app's window in front of whoever is there — the same thing a
consent request does, for the same reason: a banner behind another window is
not a warning.

`REBOOT_WARNING_SECS` is 10 seconds, deliberately the same number as
`DISPLAY_MODE_CONFIRM_TIMEOUT_SECS` rather than a second invented constant for
"how long a host has to notice something drastic".

**The grant is read again when the window closes, not when the request
arrived.** That is what makes the window a decision rather than a delay: a
revoke, or a session ending, in between keeps the machine. Withdrawing the
grant mid-countdown also takes the banner down on the spot, because a countdown
left on screen after the host changed its mind is a lie about what is going to
happen.

The warning is unconditional. There is no trusted guest for whom it is skipped,
and no setting that turns it off — `Shutdown` in particular ends every future
session until somebody is physically present, and the guest's own interface
says so before it sends.

### 4. The system's own shutdown path, and a refusal is an answer

`shutdown.exe /r|/s /t 0` on Windows, `systemctl reboot|poweroff` on Linux with
a `shutdown -r|-h now` fallback, `shutdown` on macOS. Every one of them runs the
machine's real shutdown sequence — other sessions warned, services stopped in
order, filesystems flushed — and every one of them refuses a caller without the
rights. Writing our own would mean reimplementing that badly and losing the
refusal.

`/f` is deliberately absent on Windows. Forcing applications closed discards
unsaved work belonging to the person at that machine, who did not ask for any
of this; a shutdown an open document can veto is the correct failure here, and
it is reported rather than swallowed.

Every outcome — refused for want of the grant, warned, cancelled, started,
failed — is one audit record (§15). For a message with no answer of its own,
the host's log is the only place a refusal is visible at all.

### 5. The guest waits; it does not resume

After the link goes away, `RECONNECT_WINDOW_SECS` runs first and unchanged: an
ordinary blip repairs itself with the session and its grants intact, and dialing
inside that window would race the repair with a *new* session carrying none of
them. Only once it has elapsed is this a machine that went away rather than a
link that stuttered, and only then does the wait begin.

The wait is a flat `REBOOT_WAIT_RETRY_SECS` (15 s), not a backoff. A machine
coming back from a restart is not a congested server: it is unreachable and
then abruptly reachable, so what matters is how soon after that moment the next
attempt lands — which is exactly where a doubling interval is at its worst. The
cost is bounded by `REBOOT_WAIT_CEILING_SECS` (10 minutes), after which the
ordinary "connect again" button is what is left. A const assertion keeps the
ceiling above the resume window, because the two numbers mean opposite things.

**What comes back is a new session.** New consent, or a new device password —
never the grants the old one held. This is the point the whole ADR is arranged
around, and it is a test, not a comment.

### 6. Reconnecting unasked needs both halves

`may_auto_reconnect` is `history.is_trusted(tag) && remembered_password(tag)`,
and neither half is enough alone.

The trusted flag is set by nothing except somebody setting it — not by
connecting, not by being granted a role, not by saving a password — so a host
is only ever dialed unasked because a person said it may be. The remembered
password is the other half because an attempt that ends at a prompt nobody is
there to answer is a dial loop rather than a reconnection (ADR 0044).

## Consequences

- A host can hand a guest full control of the keyboard and still be certain the
  machine will not go down without ten seconds and a button.
- A guest that restarts a host loses everything it was granted, every time, and
  gets it back only by asking again or by holding that host's device password.
  Sessions, grants and chat history do not survive a restart, by design.
- `Shutdown` towards a machine nobody is near is a way to lose remote access
  until somebody walks to it. The grant covers it, the interface says so, and
  that is as far as software can go.
- A host whose OS refuses the shutdown — a policy, an open document, a missing
  right — stays up and says why, in its log and its audit trail. The guest sees
  a machine that is still there, which is the same thing it sees for a refusal.
- `system_power::go_down` refuses outright in a test build. The tests that
  matter drive the real host path against real actors, and one that lost a race
  would otherwise restart the developer's machine mid-suite.

## Alternatives considered

- **Stretch `RECONNECT_WINDOW_SECS` to cover a restart.** The one change this
  ADR exists to refuse. That window's length is what makes carrying grants
  across it defensible; a version of it long enough for a reboot is a set of
  grants outliving the machine that agreed to them.
- **Persist grants across a restart.** Same objection, stated more honestly.
  Consent was given by a running session on a machine in a known state; after a
  restart neither the session nor the state exists.
- **Let `input` imply a reboot.** Somebody with the keyboard can type
  `shutdown -r` anyway, so the grant looks redundant. It is not: what the grant
  buys is not difficulty, it is the host's ability to say no to this one thing
  and yes to the rest, and the record of which it chose.
- **Acknowledge the request on the wire.** There is no honest moment to send it.
  Before the warning it promises something the host has not decided; after it
  the machine is going down and the packet loses the race.
- **Let the guest set the warning length.** A guest that can set it can set it
  to zero, and the warning is the whole decision.
- **An exponential backoff for the wait.** Optimizes for a server under load,
  which is not the failure being waited on; it is slowest exactly at the moment
  the machine comes back.
- **Reconnect unasked to any host with a saved password.** Saving a password is
  a convenience decision, not permission to dial a machine unprompted. The
  trusted flag is that permission, and it is set on its own.
