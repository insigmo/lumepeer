# ADR 0148 — A host restarting into an update says so, and its guest dials the new process at once

Status: accepted
Date: 2026-10-08

The owner, connected to a host, saw that an update was available and pressed
"install" on the host. The installer is quick, they said, but the reconnect
afterwards takes a long time.

## Context

Two things were behind "a long time", and only one of them was still there.

**Five minutes, fixed already.** On 2026-10-07 the guest's log shows the same
sequence twice (03:10 and 08:58 UTC): the session drops, the guest tries to
resume it, the new host process refuses ("the host did not resume the
session"), and the guest then sits out the rest of the 300-second resume
window before it dials a new session. ADR 0144 made a refused resume dial a
new session at once; that shipped in v0.0.135.

**About twenty seconds, on v0.0.135.** The owner updated `beta` from the
reference machine on 2026-10-08. Both logs, UTC:

| time | side | what |
|---|---|---|
| 08:44:03.8 | host | `installing an update` (download starts; the picture still runs) |
| ~08:44:08.7 | host | the process exits; the guest's last packet from it |
| 08:44:11.3 | guest | `the host went quiet: knocked again` (nothing answers) |
| 08:44:14.6 | host | the new process starts logging |
| 08:44:16.5 | host | the restored invite is served on the obfuscated transport again |
| 08:44:17.3 | guest | QUIC idle timeout: `waiting for a host to come back`, resuming, first attempt in 3 s |
| 08:44:20.3 | guest | resume dial |
| 08:44:21.0 | guest | `the host did not resume the session` |
| 08:44:22.0 | guest | new-session dial (ADR 0144's one second) |
| 08:44:22.9 | guest | consent granted by the unattended proof |
| 08:44:24.0 | guest | codec negotiated; the picture is back |

The installer and the new process's start take about eight seconds, and
nothing in this app can make them shorter. The rest is waiting that does not
have to happen:

- **The guest learns of the restart from a timeout.** The Windows updater
  starts the installer and calls `std::process::exit(0)`; nothing tells the
  peers. The guest sees a frozen picture with no banner for the 8 s of
  `QUIC_MAX_IDLE_TIMEOUT_SECS`. On this run the timeout happened to end just
  after the new process was up. With a slower installer it ends long before,
  and the guest then spends its first dials on a host that is not there.
- **The guest resumes a session nobody can resume.** The process that
  comes back has never heard of the session. The resume costs the 3 s first
  tick, the dial and its refusal, and ADR 0144's second, about five seconds in
  all.
- **A dial at a host that is not up yet is not heard when the host comes
  up.** On the obfuscated transport the host only punches towards a guest
  that knocked, and the signalling relays keep no knock for later (the event
  kind is ephemeral; `nostr::subscription` asks only for live events). A
  dial's single knock, sent while the host is down, is lost. The host that
  starts mid-dial hears this guest only at the dial's next attempt, up to a
  whole 12.5 s punch train later.

## Decision

1. **The host says it is restarting.** `ActorHandle::announce_restart` closes
   every connection with a new application close code, `CLOSE_RESTARTING`
   (9, reason `RESTARTING`), and returns once `RESTART_CLOSE_FLUSH_MS` (250 ms)
   has given the closes time to leave. From then on, a control connection that
   finishes its handshake with this process is closed the same way instead of
   being given a session that would die with the process a moment later.
   - Windows: called from the updater's `on_before_exit` hook, which runs
     after the installer is on disk and right before the exit. The hook is
     synchronous and runs inside `install` on a worker of the async runtime;
     `block_in_place` lets that worker wait while the actor does the work.
   - Linux: called after the package is installed and before
     `request_restart` (ADR 0119).
   - macOS: not called. The updater replaces the bundle under the running
     process and nothing restarts it, so there is nothing to announce.
2. **The guest dials the new process instead of resuming.** A session that
   ends with `CLOSE_RESTARTING` starts the wait of ADR 0084 for a *new*
   session, as a refused resume does (ADR 0144): no resume claim, no resume
   window, the first dial after `HOST_RESTART_REDIAL_SECS` (2 s), the parked
   window kept for the session to come back into (ADR 0105). A wait's own
   dial that reaches the old process on its way out, and is turned away with
   the same code, starts the same wait again, even when the dial had already
   ended the wait.
3. **A dial that hears nothing knocks again.** `knock_on_silence` (ADR 0134)
   counts a route that has heard nothing yet as silent from the dial's own
   knock, so a dial at a host that is not answering knocks every
   `RENDEZVOUS_REPUNCH_SECS` (3 s) for as long as it has a connection open.
   A host that comes up mid-dial hears the next knock, punches and answers,
   and the answer starts a fresh attempt at once (ADR 0116).

## Consequences

- From the click, the picture freezes for the installer and the new
  process's start, plus a second or two to dial and be let in. The guest
  shows "reconnecting" at once instead of a frozen picture with no banner.
- An older guest receives a close code it does not know. It ends the session
  as it would after a timeout, only sooner, and resumes as before.
- A dial at a host that never answers sends a knock every 3 s instead of one
  per attempt. That includes the presence check of ADR 0127 at a host that is
  off: about four small relay events per 10-second attempt instead of one.
- The new session is still a new session. A guest the host lets in by an
  unattended proof (ADR 0123) is back without anyone doing anything. Any
  other guest is asked for consent again, by whoever is at the host, as
  ADR 0106 already does after a reboot.
- If the Windows installer fails to start after the hook ran, the process
  exits anyway (`tauri-plugin-updater` ignores `ShellExecuteW`'s result), as
  it did before. Nothing new is left half-done.

## Verification

- `a_host_restarting_into_an_update_tells_its_guests`: a host actor closes a
  waiting guest with `CLOSE_RESTARTING` and turns away a guest that arrives
  after it. Fails without the guard.
- `a_host_restarting_into_an_update_is_dialed_again_at_once`: a guest whose
  host closes with `CLOSE_RESTARTING` dials again within
  `HOST_RESTART_REDIAL_SECS` + 3 s with no resume claim, and once more after a
  dial that reached the old process. The new session comes back into the old
  window. Fails without the guest change: the first dial carries the old
  session's claim.
- `a_dial_that_hears_nothing_knocks_again`: a dial at an address nobody
  answers knocks a second time 3 s after its first. Fails without the change:
  one knock for the whole punch train.
- e2e only: with `LUMEPEER_E2E_FAKE_UPDATE` set, a pilot build's
  `update_install` runs the Windows hook and exits, with no release to
  install (`silent` exits without the hook, the way older builds exit).

Measured 2026-10-08 with that switch: the reference machine as the guest, the
host in WSL Debian on the same machine, the host started again 4 s after it
exited (an installer's worth), three runs per arm, both arms the same build.
`beta` could not be the host: its installed client held the host role during
the owner's live session.

| arm | guest notices | new session asked for | dials | resume tried |
|---|---|---|---|---|
| `silent` (as before) | 34–46 s | 39–51 s after the exit | 2 | yes, refused |
| `announce` | 0.05 s | 8.4 s after the exit, 1.0 s after the host was up | 1 | no |

The host was serving again 7.1–7.4 s after the exit in every run. On this path
the session rode iroh, and a dead host went unnoticed far longer than the 8 s
of the obfuscated transport (the guest's iroh connection moved to the relay
path and stayed open). The obfuscated path between two real machines was not
re-measured with the change; on 2026-10-08's real update it took 15.5 s from
the exit to the picture, of which about 8 s were the installer and the new
process's start.
