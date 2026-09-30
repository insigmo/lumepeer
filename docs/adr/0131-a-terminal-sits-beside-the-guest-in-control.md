# ADR 0131 — A terminal sits beside the guest in control

Status: accepted
Date: 2026-09-29

Amends [ADR 0101](0101-a-terminal-session-is-the-same-session-with-no-media-connection.md)
(decision 1, and the consequences "the host still registers a viewer at the
grant" and "two windows onto the same host cannot both exist") and the
single-controller rule of design doc §8.2 as it applied to a shell.

## Report

"If I am connected to a host, I cannot connect to its terminal. I want that:
one connection with remote control at a time, but as many terminal
connections as anyone likes."

Asked back, the owner settled three things: any guest, not only the one
already connected; terminal sessions do not count against the plan's guest
limit; and the ceiling stays at four.

## What stood in the way

Two refusals, on two sides, for two different reasons.

- **The guest refused itself.** `Actor::spawn_dial_as` refuses a dial to a
  host this node already has a connection to (`NetError::AlreadyConnected`):
  the host keeps one connection and one session per guest, and a second dial
  would replace the first and tear the working session down with it.
- **The host could not tell a shell from a screen.** ADR 0101 made a terminal
  session the same session with no media connection, deliberately without
  putting the guest's intent on the wire. So a terminal session was a
  full-control session like any other: it held the controller role
  (`SessionManager::grant` refuses a second controller) and a place under the
  plan's guest limit (one on Trial and Pro).

## Decision

### 1. The guest says what it came for, and the host gives it less

A guest dialing for a shell adds `terminal-only` (`FEATURE_TERMINAL_ONLY`) to
its `Hello.features`. A host that knows the string records the peer and asks
for consent, and grants, with `SessionKind::Terminal`:

- the session holds `Grants::for_terminal(role)` — `terminal` when the role
  carries it (full control), and nothing else: no `view`, so the host refuses
  `rd/media/1` and never registers a viewer; no `input`; none of the
  independent grants, which the host can still switch on one at a time;
- no allowlisted `ControlLimited` actions, which are input by another name;
- it is not the controller (`SessionManager::controller` reads screen
  sessions only), and it takes no place under the plan's guest limit;
- at most `MAX_TERMINAL_SESSIONS` (4) of them run at once, each still held to
  `MAX_TERMINALS_PER_SESSION` shells.

ADR 0101 rejected a flag like this as "a flag the host *trusts*". This one is
not trusted: its only effect is to make the session smaller than the role
would, so a guest that lies about it gets less, never more. That is why it
can sit beside the controller — a host running one controller and four
shells has still let exactly one guest at its screen and its keyboard.

No new message and no `PROTOCOL_MINOR` bump. An older host ignores the
string (§9.1) and admits the guest for the screen, or refuses it, exactly as
before. An older guest never sends it.

The consent dialog says the request is for a terminal, and offers Deny and
"Allow terminal", which grants full control — the only role that carries a
shell — for the host to cut down to the shell. The host's own card and
session bar call such a session "terminal only" rather than by its role.

### 2. A guest that is already connected opens a window, not a connection

"Connect to terminal" on a host this node already has a session with opens a
terminal window beside it (`term-{peer}`, `terminal.html`), the way ADR 0124
opens a file manager beside a session instead of dialing again. Its shells
ride the session that is already there: they are the same session's shells,
under the same grant and the same `MAX_TERMINALS_PER_SESSION`.

One per session; a second click raises it. A session that is itself a shell
also gets one, since that click asked for another shell and its own window
has room for one.

The session's shells share one channel and one queue of things to tell the
guest, and now two windows poll it. So the actor keeps them apart
(`TerminalWindow::{Session, Beside}`): each open remembers which window asked
— the host answers opens in the order they were sent and names no ask, so the
order is the whole of the pairing — each shell remembers its window, and each
poll drains only its own window's queue. The terminal commands tell the
windows apart by label (`check_terminal_window`), and the new window's
capability lists the five terminal commands and nothing else.

Closing the window ends its shells and not the session. The page asks, and
the window's own destroy handler asks again through
`ActorHandle::terminal_window_closed`, because Tauri may destroy the window
before the page's request leaves; an ask still in flight when the window
goes is marked, and the shell it answers is closed the moment it arrives.
Closing the session's own window closes this one with it.

## Consequences

- A guest in control and up to four shells, from any guests including the
  one in control, run at once — on every plan.
- ADR 0101's last consequence is gone: a terminal session holds no `view`,
  so on a host that knows the string the capture backend is never started
  for it, and a macOS or Wayland prompt no longer appears for a session that
  will never show a picture.
- A terminal session connected from a saved host's menu no longer has the
  file manager, clipboard or tunnel it had under ADR 0101 unless the host
  switches them on. A shell was what it asked for.
- A guest holding a terminal session that asks for the screen is a new
  screen guest, held to the plan and the controller like one; the guest still
  refuses that second dial locally, as before.
- The terminal's window is still 80×24 in both places; fitting the emulator
  to its window is its own change.

## Alternatives considered

- **A `Role::Terminal`.** A wire change: `Role` rides `Hello` and
  `ConsentGrant` through postcard, and an appended variant is a parse failure
  on every older peer — the reason ADR 0101 gave, and still true.
- **Keying the host's sessions by (guest, kind), so one guest could hold a
  screen session and a terminal session on two connections.** Every per-peer
  map in the actor — connections, media, files, tunnels, shells, the resume
  window — is keyed by the guest alone; splitting them is a far larger change
  than a window, for the one case where the guest already has a session its
  shells can ride.
- **Counting shells rather than sessions against the four.** The per-session
  bound already exists and stays; what was new was sessions that hold no
  place anywhere else, and that is what needed a ceiling.
