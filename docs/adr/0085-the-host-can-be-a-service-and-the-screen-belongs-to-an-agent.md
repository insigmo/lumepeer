# ADR 0085 — The host can be a service, and the screen belongs to an agent

Status: accepted
Date: 2026-09-15

Answers the half of `docs/tasks/14-release-infrastructure.md` task 4 that
[ADR 0043](0043-one-privileged-helper-with-one-capability.md) explicitly did
not deliver ("this means the 'runs before anybody signs in' half of task 4 is
not delivered"), and that `README.md`'s "Written but not wired" has named ever
since: *a session-0 process that hands capture and injection to whichever
session exists.*

Extends ADR 0043 (one privileged helper), ADR 0049/0056/0057 (the worker that
crosses the session boundary), ADR 0033 (unattended admission), ADR 0034 (the
address book as the gate) and ADR 0063 (both ways in are offered at once).
Scope here is the **foundation**: a host that survives a sign-out and a
sign-in. Reaching the *logon screen itself*, and following a fast user switch,
are a separate capability with a separate decision.

## Context

Everything this project ships as a host today runs inside one interactive
user's session: `apps/desktop/src-tauri` binds the endpoint, owns
`SessionManager`, captures the screen and injects input, all as that user.
That is why there is no remote access to a machine at its lock screen, and why
signing out ends a session that a remote guest was legitimately admitted to.

Two mechanisms already exist and decide most of the shape before any new code
is written.

**The privileged side already knows how to run code in another session.**
`crates/service/src/secure_desktop_launch.rs` duplicates the service's own
`LocalSystem` token, stamps the duplicate with the console session id
(`SetTokenInformation(TokenSessionId)`, which needs `SeTcbPrivilege`), and
`CreateProcessAsUserW`s *this same binary* into that session with one fixed
argument and a named desktop. ADR 0056 built it for a worker that lives
milliseconds. Generalizing that worker into one that lives for a session is
the mechanical core of this ADR, and nothing about the technique is new.

**Nothing about the network layer needs a window.** The actor in
`apps/desktop/src-tauri/src/network.rs` is 19 145 lines that mention `tauri`
six times, all six in the start-up glue: `spawn_actor`'s `AppHandle`
parameter, and three path helpers that ask Tauri where the app data directory
is. The window surface it does use is already a seam —
`crate::view::ViewWindows`, a trait with a real implementation and a detached
one — put there so the actor could be driven in tests without a webview. The
work is therefore *extraction*, not a rewrite: the same actor, behind the same
seam, with a second front end.

Two facts constrain the answer and are worth stating before the decisions.

- **A privileged process that holds the network is a different animal from
  ADR 0043's helper.** The helper's security argument is a wire with three
  fixed-shape members, and it is a good argument precisely because it is so
  small. It does not survive adding QUIC, a media pipeline and a keystore to
  the same process, and pretending otherwise by writing "the same argument
  applies" would be the dishonest move this ADR log exists to prevent.
- **"No hidden capture" is the ground rule that this feature strains.** It is
  enforced today by a banner on the host's own screen. A host with nobody in
  front of it has no screen to put a banner on, and the tempting answer — ship
  the capability and note the gap — would make the rule conditional on
  somebody being there to enforce it. It is not.

## Decisions

### 1. What moves to SYSTEM: the network, the session, and nothing that touches a screen

The split is the one the task file recommends, stated as a rule rather than a
preference:

- **In the privileged process:** the Iroh endpoint, the handshake, the invite
  registry, `SessionManager`, consent, grants, unattended admission, the
  address book and the audit log — that is, `lumepeer-core` plus
  `lumepeer-net` plus the actor that owns them.
- **In the agent:** capture, encode, input injection, the clipboard, the host's
  own session bar and every other thing that is a fact about a desktop. The
  agent is an ordinary process in the active console session, running as the
  signed-in user, launched by the privileged side.
- **The privileged side draws no pixels and presses no keys.** Not "does not
  today": it holds no capture backend and no injector, and the only way a
  pixel or an event crosses the boundary is the agent channel of decision 3.

The reason is not tidiness. A `LocalSystem` process that calls `SendInput` is
a process whose compromise is the machine; a `LocalSystem` process that
forwards an already-authorized event to a user-session agent, which performs
it with exactly the rights of the person at the screen, is not. The agent gets
its token from `WTSQueryUserToken`, not from the service's own token — it is
the signed-in user, not SYSTEM, and it can do nothing that user could not.

**This is a second service, not a third capability on `LumepeerHelper`.**
`LumepeerHelper` keeps the three fixed-shape opcodes ADR 0043/0049/0057 gave
it, and this ADR does not add a fourth. The session-0 host is its own service,
`LumepeerHost`, because the two have different threat models and only one of
them can be defended by enumerating its wire:

| | `LumepeerHelper` | `LumepeerHost` |
| --- | --- | --- |
| Reachable from a local unprivileged process | yes, by design (`IU` on the pipe) | **no** |
| What a local caller can ask for | three fixed-shape operations | nothing; it has no local request endpoint |
| Network | none | the endpoint, with the same authorization path the client has today |
| Its local channel | the DACL'd request pipe | the agent channel, which carries no request the agent can make |

Merging them would put the network behind a pipe that interactive users may
open, which is the one shape ADR 0043 spends its whole length avoiding.

**The actor is extracted, not copied.** `network.rs` and the Tauri-free
support modules around it move into a library crate that both front ends link,
so there is exactly one implementation of "who may do what" on this machine.
A second copy of the consent logic, kept in step by review, is how a host ends
up authorizing two different things depending on which process answered. The
extraction is deliberately shaped so the result is usable by a client that has
no desktop at all — pack `27` (the guest core as a library) lands on this
crate rather than repeating the work for mobile.

### 2. Who admits a guest when nobody is at the machine: unattended, or nobody

Unchanged from ADR 0033 and ADR 0034, and narrowed rather than relaxed:

- The only credential path is the Argon2id device password plus, when
  configured, the RFC 6238 code. `UnattendedAccess::admit` is still the whole
  decision and still hands back a `Role` rather than a boolean.
- The address book is still the gate (ADR 0034): a device absent from it is
  never trusted, a corrupt book is an empty book, and a book that cannot be
  persisted trusts nobody.
- ADR 0063's "both ways in are offered at once" still holds **whenever there
  is somebody to offer it to**. With an agent alive, a session-0 host queues
  the consent request exactly as the client does today and the agent shows it.
  With no agent, there is no dialog, and the credential path is the only way
  in.

And the rule that makes this safe to ship, stated as a refusal rather than a
default:

> **A host with no unattended credentials configured admits nobody while it is
> the service that is hosting.** Not "falls back to asking" — there is nobody
> to ask. Not "allows the address book to stand in" — trust narrows who may
> *try*, it never substitutes for a factor. The connection is accepted,
> answered honestly, and closed.

This is deliberately a worse experience than guessing, and it is the only
behaviour that keeps "anything not explicitly permitted is forbidden" true on
a machine with an empty chair in front of it. It is unit-testable without any
hardware, and it is tested.

### 3. "No hidden capture" with nobody to watch the banner

Three mechanisms, all of them structural rather than advisory.

**a. There is no capture without an agent, and no agent without a session.**
The privileged process holds no capture backend at all (decision 1). A guest
admitted while nobody is signed in is admitted to a session with no media: it
gets the existing honest `MediaUnavailable` state, not a frozen frame and not
a black one. So "pixels left this machine at a moment when no banner could
have been shown" is not a state the code can reach — not because a check
forbids it, but because the component that produces pixels only exists inside
a session that has a screen.

**b. The agent raises the banner before the first frame leaves it.** The
non-dismissable unattended indicator is the agent's first act, not a
consequence of the first frame. The order matters: an agent that started
capture and then showed the banner would have a window, however short, that
is exactly what §2.2 forbids. Concretely, the host-bar/indicator instruction
is a message the privileged side sends the agent when the agent attaches, and
capture is only ever started afterwards.

**c. The moment somebody arrives is the moment they are told.** This is the
answer to the case the ground rule is really about: a guest is let in with
nobody present, and half an hour later the owner signs in. Signing in creates
the console session; the privileged side notices and starts the agent; the
agent's first act is the banner. So the person who walks up learns within one
sign-in, from a control they cannot dismiss, that a session is live — and it
is live *before* they can be observed, because (a) means nothing was captured
while they were away.

**d. Every admission with nobody present is audited.** `AuditEvent::
UnattendedLogin` already records the verdict of a credential admission and is
already written by the actor that moves into the privileged process, so this
costs nothing new and survives the reboot the log is read after. What this ADR
adds is that it is the *only* way in when there is no agent, so the log is
complete by construction rather than by diligence.

### 4. One host per machine, and the switch between them is an act

Two hosts on one machine is not a degraded mode, it is a contradiction: two
processes would each believe they own `SessionManager` for the same screen,
and the `ControlLimited` snapshot of §8.2 would be taken twice against two
different policies. So:

**The host role is a single machine-wide token, and whoever holds it is the
host.** It is a named kernel object rather than a file or a port, because the
kernel releases it when its holder dies — a crashed host must not leave the
machine permanently unhostable — and because a mutex cannot be half-acquired
the way a lock file can be half-written.

- `LumepeerHost` takes the token when it starts hosting and holds it for as
  long as it does.
- The desktop client asks for the token before it spawns its actor. If the
  service holds it, the client **does not become a host**: it starts, it is
  still a guest, and its host surface says which process is hosting instead of
  silently competing.
- The handover is one explicit operation in one direction: the client, which
  already runs elevated (ADR 0057), signals a named event the host service
  waits on; the service ends its sessions, releases the token, and stops
  hosting until asked to resume. The event carries no parameters, has no
  payload to validate, and is admitted to `LocalSystem` and administrators
  only. There is nothing for a peer to ask for, and nothing for a local
  unprivileged process to reach.

The direction is deliberate. A remote guest cannot cause a handover in either
direction: the token is taken by processes on this machine, and the event is
signalled by a person at this machine with administrator rights. A guest
admitted to the service's host cannot promote itself into the user's client,
and a guest connected to the user's client cannot push it aside to reach the
service.

**Why the person at the machine wins the tie.** The client asks, and the
service yields when asked, rather than the reverse: the client is the surface
that shows who is connected and carries the revoke, and a design where a
remote party's session outranks the controls of the person sitting in front of
the machine is the wrong direction for this project regardless of how
convenient it is.

## What this ADR does not decide

Named rather than left to be rediscovered, because the temptation to fold them
in here is exactly what would make this change too large to review:

- **The logon screen** (`Winsta0\Winlogon` as a session, not as a one-frame
  worker) and **following a fast user switch**. Serving a machine at which
  *nobody has ever signed in* is strictly more than surviving a sign-out: it
  discloses a desktop that belongs to no session, and it is a new capability a
  guest can use against a host. It gets its own grant and its own ADR in pack
  `26`. Until then a session-0 host with no interactive session serves control
  and no picture.
- **No new `IndependentGrant`, and the reason.** Nothing in this ADR widens
  what a guest may do. The same roles carry the same eleven flags against the
  same `SessionManager`; what changes is which process owns it. A grant added
  here would be a flag no guest could exercise, which is worse than no flag at
  all — it would suggest the capability is gated when the gate does nothing.
  The capability that *is* new arrives with the logon screen, above.
- **Linux and macOS.** No daemon, for ADR 0043 §7's reason restated: those
  platforms have their own session mechanisms, and a root daemon built by
  analogy with the Windows one would hold privileges for a design nobody has
  worked out yet.
- **A webview in session 0.** There is none, and there is no path to one: the
  privileged process links no UI toolkit.

## Consequences

- A machine that has installed Lumepeer can have two services rather than one.
  That is a real addition to its attack surface and the mitigation is the
  table in decision 1, not an argument that it is small: `LumepeerHost` holds
  the network, and its defence is that it exposes no local request endpoint
  and authorizes with the same `lumepeer-core` the client already uses.
- **The keystore in SYSTEM is a machine keystore, not a user one.**
  `crates/net`'s native backends are all per-user (Credential Manager, Secret
  Service, Keychain) and a `LocalSystem` service has no meaningful user
  keyring. The host service therefore uses the existing encrypted
  `FileKeystore` under a machine path, in a directory whose ACL admits
  `LocalSystem` and administrators and nobody else.

  DPAPI with `CRYPTPROTECT_LOCAL_MACHINE` was considered and rejected on the
  merits rather than on effort: a machine-key DPAPI blob can be unprotected by
  **any** process on the machine, so it would add FFI (in a crate that is
  `#![forbid(unsafe_code)]`) in exchange for no boundary the directory ACL
  does not already draw. The honest statement of the protection either way is
  ADR 0043's and §3.1's: this is not a defence against a local administrator.
- **Machine paths, not the user profile.** The host service's `ActorStores`
  point at a machine-wide directory. A privileged service writing into
  whichever profile happened to be first is how one user's address book
  becomes the machine's policy.
- The desktop client gains a state it did not have: *running, not hosting*.
  Every surface that assumed "this app is the host" has to mean "this app is
  the host **if it holds the token**".
- `crates/service` grows from three narrow capabilities to three narrow
  capabilities plus a long-lived child process and a channel to it. The child
  is the part that needed the most care, and the rule it is built around —
  written into `crates/service/src/agent_protocol.rs` and enforced by the
  message set having no variant that could break it — is that the agent is
  *told what to do* and never *asked what is permitted*. There is no message
  an agent can send that widens a grant, names a peer, or decides anything;
  the authorization is made in `lumepeer-core` before the channel is touched,
  exactly as ADR 0049 §4 and ADR 0057 §4 already require of the worker.

## Verification

What is provable without a second machine, and is:

- The agent channel's message parsing: every field bounded, every malformed
  input refused rather than guessed, no panic on any byte sequence.
- The refusal path: a host service with no unattended credentials configured
  admits nobody, and says so the same way every other refusal does.
- The host-role token: it is exclusive, it is released when its holder exits,
  and a second acquirer sees it held.
- The agent lifecycle state machine: no agent means no capture and an honest
  media state, and an agent that dies is noticed rather than assumed alive.

What needs hardware, and is therefore a `docs/release-checklist.md` step
rather than a test any contributor runs — for ADR 0043's own precedent, that
installing a `LocalSystem` service does not belong in an automated suite:

- A guest connecting to a host whose session belongs to the service, with the
  agent serving picture and input from the user's session.
- Signing out killing the agent, and the guest seeing an honest state rather
  than a frozen frame; signing back in bringing the agent and the banner back.
- Both services installed, and the user client refusing to become a second
  host while the service holds the token.
- The ACL on the machine store directory, read back on a real install.
