# ADR 0046 — The helper service gets a second capability: secure-desktop frames

Status: accepted
Date: 2026-09-01

Extends ADR 0043. Implements `docs/bugs/15-secure-desktop-capture.md`
(`DECISIONS.md`, D8, second half — the honest-degradation half of D8 is
ADR-less and shipped in `docs/bugs/11-uac-degradation.md`).

## Context

ADR 0043 is titled "one privileged helper with **one** capability" and means
it: the wire between an unprivileged client and the `LocalSystem` service is
two bytes in, two bytes out, and the one thing it can ask for is "deliver
Ctrl+Alt+Del". That narrowness is the whole security argument — a
`LocalSystem` process reachable from an unprivileged one is a local privilege
escalation waiting to be written, so the smaller the set of things it can be
asked to do, the smaller the cost of it being compromised, or of its DACL
being wrong.

D8 asks for a second thing: showing the guest the actual secure desktop
(`Winsta0\Winlogon` — the UAC consent prompt, the lock screen, the
fast-user-switch screen) instead of the honest "can't see this, will resume"
message `docs/bugs/11-uac-degradation.md` already ships. Only a process
running as SYSTEM in session 0 that has called `SetThreadDesktop` on that
desktop object can see it; `crates/service` is already that process, minus
the capability and a way to get pixels out of it.

**What this means in the plainest terms, because the task file demands
plainness here:** if this is built and turned on for a guest, that guest can
watch the host type an administrator password into a UAC prompt — not the
characters (the field still masks them as dots; only the machine can distinguish
between an empty vs. a non-empty combo box, timing of keystrokes, and any
of the surrounding chrome), but the whole visible dance of it, live, at
whatever quality the video pipeline is running. That is the single most
sensitive thing this codebase has been asked to transmit, and the rest of
this document does not get to look away from that by discussing wire formats
instead.

## What the privileged helper can now see, and why that is acceptable

Before this ADR, `crates/service` executes exactly one Win32 call
(`SendSAS`) and never reads anything back. After it, on request, it also
switches its calling thread onto `Winsta0\Winlogon` and takes a GDI
screenshot of whatever is rendered there. That is strictly more capability,
and pretending otherwise would defeat the point of writing this down.

It is acceptable only under all of the following, none of which is
negotiable on its own:

1. **The request still has to come from somewhere that already decided to
   allow it.** The service does not gain judgment; it gains one more thing
   it will do when asked by someone the pipe's access list admits, exactly
   as `SendSAS` already is. The actual authorization — *should this guest,
   in this session, see this* — is made in `lumepeer-core`, once, before the
   pipe is ever touched, by a grant introduced below that is off by default
   and independent of every role.
2. **The blast radius of the service being compromised does not grow as
   much as "it can now take a screenshot" suggests**, because of a
   mitigation this ADR adds specifically for this operation (see
   "Session binding" below) that `OP_DELIVER_SAS` does not have and does
   not need.
3. **Everything through this path is additive.** `docs/bugs/11-uac-degradation.md`'s
   honest-message-plus-auto-recovery behaviour is not touched by a single
   line and is the answer whenever the service is missing, the grant is
   off, the session check fails, or the capture itself fails for any
   reason. Nothing about this feature can make the existing degradation
   path worse, because the new path only ever runs *before* falling back to
   the old one, never instead of being able to.
4. **The host is told.** For the whole duration a guest is actually seeing
   secure-desktop pixels, the host's own window shows a marker it cannot
   turn off, on the same mechanism the recording indicator already uses
   (§17). A host who never notices a UAC prompt fire while a guest was
   already connected loses nothing they had before this feature; a host
   who is watching their own screen learns immediately that this happened.

None of this makes the underlying fact go away: a host who grants this to a
guest they should not have trusted has handed that guest a window onto their
own elevation prompts. That is the actual scope of D8's approval, worded
plainly rather than softened — the host decides, on a session it already
otherwise trusted enough to have granted `view`, and the safety net is that
the decision is explicit, revocable, visible to the host while it is
happening, and off by default.

## Decisions

### 1. A new, independent grant: `secure_desktop`

`Grants` (`crates/core/src/consent.rs`) gains a fifth field,
`secure_desktop: bool`, and `IndependentGrant` gains `SecureDesktop`. It
follows the exact shape of `clipboard_read`, `clipboard_write`,
`file_transfer` and `recording`:

- `Grants::from_role` never sets it, for any role, `FullControl` included —
  the same "`FullControl` does not imply recording or files" ground rule,
  applied here because a guest asking to control the mouse and keyboard is
  a different decision from a guest asking to watch an administrator
  authenticate.
- It moves only through `SessionManager::set_grant`, which already refuses
  anything but an `Active` session and snapshots nothing that survives a
  revoke — the same machinery every other independent grant already uses,
  so the "not derivable from role", "deny by default" and "revoked
  immediately, cleared on the next `grant`" properties come from the
  existing generic tests over `ALL_INDEPENDENT` rather than from new,
  bespoke logic that could disagree with the other four.
- The UI label is not "secure desktop": it is "See the administrator
  prompt and lock screen" (`status.grants.secureDesktop`), because a host
  deciding whether to flip this switch needs to understand the
  consequence, not the Windows term for the mechanism.

### 2. One new opcode, same shape, plus one narrow side-channel

`crates/service/src/protocol.rs` gains `OP_CAPTURE_SECURE_DESKTOP`. The pipe
frame shape does not change: two bytes of request in
(`[MAGIC, OP_CAPTURE_SECURE_DESKTOP]`, no parameters — not "capture desktop
named X", there is exactly one desktop this operation ever means), two bytes
of response out (`STATUS_OK` / `STATUS_REFUSED`, same meaning as today:
"ask again" carries no more information than that).

`STATUS_OK` alone cannot carry a screen's worth of pixels, and the whole
point of §2's fixed frame is that it never has to grow a length field to try.
So a second, separate, fixed-capacity channel carries the pixels: a single
named shared-memory mapping
(`crates/service/src/frame.rs`, `Global\lumepeer-secure-desktop-frame` on
the Windows side), sized once at compile time
(`SECURE_DESKTOP_FRAME_CAPACITY_BYTES`, one BGRA8 frame at the pipeline's
existing `1920×1080` ceiling — the same bound `MAX_PICTURE_PIXELS` already
puts on every other frame this codebase moves) and never resized at
runtime. The service is the only writer; interactive users get read-only
access, mirroring the pipe's own DACL (`SY`/`BA` full, `IU` read-only,
`GR` rather than the pipe's read/write — there is nothing for a client to
write here at all).

This preserves the property ADR 0043 is actually about: the *shape* of what
can be asked for and answered is fixed before either side ever runs, not
negotiated at runtime from something the peer supplies. A malicious or
buggy client cannot make the service allocate more, expose more, or listen
on anything new — the mapping exists (or does not) regardless of what
anyone asks, at one size, for one purpose.

**Ordering, and why no sequence number or lock guards the mapping.** The
mapping is written to completion by the service *before* the pipe's
`STATUS_OK` is written, and a client never opens the mapping until *after*
its blocking read of that reply has returned. Two `WriteFile`/`ReadFile`
kernel calls on the same named pipe are what stands between "service
finished writing pixels" and "client starts reading them" — that round trip
is already a stronger ordering guarantee than an application-level lock
would add, so this ADR does not invent a seqlock or a mutex for a mapping
that is never touched by both sides at once. Each request is answered on a
fresh connection (`accept_and_serve` already disconnects after every reply),
so there is no in-flight state to reconcile between polls, only "the mapping
currently holds whatever the most recent successful capture produced."

**Why not extend `crates/media`'s decoder ring buffer (§11.3) instead of a
new mapping.** That ring buffer is a same-user, two-cooperating-process
design (the decoder sandbox `crates/media` itself spawns), sized for a
continuous stream and already carrying the decoder's own indices and
double-buffering. Session 0 talking to an interactive session is a
different problem — the mapping has to be reachable across the session
boundary the ring buffer was never asked to cross (`Global\` namespace, a
custom DACL, no dependence on either side's process being the one that
created the other) — and reusing a ring built for a different pair of
processes and a different security boundary would either weaken one of them
or bend it out of the shape that makes it reviewable. A second small,
purpose-built mapping — the same *pattern* the ring buffer already
established in this codebase (a fixed-size file-backed mapping instead of
per-frame serialization) — is more honest about what it is than a shared
abstraction pretending these are the same problem.

**Why these constants live in `crates/service`, not
`crates/core/src/constants.rs`.** Every other numeric constant in this
project lives in `crates/core` so that nothing duplicates it silently, but
`crates/service`'s own wire constants (`FRAME_LEN`, `PIPE_BUFFER_BYTES`,
`PIPE_TIMEOUT_MS`) already predate this ADR and already live locally, for a
reason ADR 0043 states directly: this crate's dependency list is part of
its security argument, and `lumepeer-core` is not on it. Adding a
compile-only dependency purely to import one constant would be a bigger
change to what a `LocalSystem` binary's supply chain looks like than the
constant itself is worth. The new constants below follow the file's
existing precedent instead: local, doc-commented, and named so that a
`grep` for `1920 * 1080` or `MAX_PICTURE_PIXELS` finds the comment
explaining why they have to agree.

### 3. Session binding: the mitigation `OP_DELIVER_SAS` does not have

ADR 0043's access list admits any **interactive** user (`IU`) — deliberately
broader than "the session at the physical console", because a Ctrl+Alt+Del
delivered to whichever session receives it is a narrow, actively-visible
effect: if it lands somewhere the caller cannot see, the caller learns
nothing and nothing was disclosed.

A screenshot is not that. `IU` on its own would let a process belonging to
*any* interactively logged-on user — a second local account, a fast-switched
session that is not the one at the screen — ask the service for a picture of
whatever the console session's secure desktop is currently showing, which is
a real cross-session information disclosure the DACL alone does not close.

So `OP_CAPTURE_SECURE_DESKTOP` adds one more check that `OP_DELIVER_SAS`
does not carry and does not need: before capturing anything, the service
reads the connected pipe client's process id
(`GetNamedPipeClientProcessId`), resolves its session
(`ProcessIdToSessionId`), and compares it against
`WTSGetActiveConsoleSessionId()`. Anything else — a different session, or
either call failing — is `STATUS_REFUSED`, with no capture attempted.

This is a mechanical property of *which desktop object a caller may even
ask about*, not a policy decision about who is allowed to run a session —
the same category ADR 0043 already put the pipe's own DACL in ("the access
list is the authorization... there is no second authorization check inside
the service, and there should not be"). `lumepeer-core` still makes the only
*policy* decision (does this guest, on this session, hold `secure_desktop`)
before the pipe is ever touched; this check only prevents the operation from
being usable as a spyglass into a screen the caller was never in front of,
regardless of what any policy decided.

**What this does not fix**, stated because the task this ADR answers to
insists on it: `WTSGetActiveConsoleSessionId` names the session attached to
the physical console. A host reached over Remote Desktop rather than sitting
at the machine has no "active console session" that is theirs, and this
check would then refuse the operation for them too — correctly failing
closed rather than guessing, but not a feature. Lumepeer's own host/guest
model already assumes the host is the interactive user physically at that
machine (the same assumption `docs/bugs/12-service-lifecycle.md`'s
`SoftwareSASGeneration` discussion and ADR 0043 both make for the same
reason), so this is the expected case, not a gap being hidden.

### 4. The service still does not decide, and still does not read
configuration, open a socket, or touch disk for this

`serve()`'s new arm answers exactly one question — "is there a fresh frame
of `Winsta0\Winlogon` right now" — and only after the session check above.
It has no notion of guests, grants, or sessions; `lumepeer-core`, in the main
process, already decided this call was worth making before the client ever
opened the pipe, exactly as §11's model requires. The mapping is
page-file-backed (`CreateFileMappingW(INVALID_HANDLE_VALUE, ...)`), not a
file on disk, so "touches no disk" survives this change too.

### 5. Extending the branch, not rewriting it

`docs/bugs/11-uac-degradation.md`'s task 3 put one signal in one place: the
`Ok(Err(MediaError::SecureDesktopActive(reason)))` arm of
`spawn_encode_loop` (`apps/desktop/src-tauri/src/view.rs`) is where the actor
already knows both "capture is stuck behind the secure desktop" and has
access to the session's live grants and the service's reachability. This ADR
adds one branch inside that arm — if the session holds `secure_desktop` and
`lumepeer_service::client::capture_secure_desktop_frame()` returns a frame,
encode and send it like any other frame, with the host indicator on for as
long as that keeps succeeding — and falls through to the existing announce/
retry behaviour the moment either condition stops holding. The arm's shape,
its backoff bookkeeping (`Recovery`, `SECURE_DESKTOP_RECOVERY_*`) and
`docs/bugs/11-uac-degradation.md`'s own fallback are untouched.

Because the secure desktop is largely static (a UAC dialog, a lock screen),
this path is polled at `SECURE_DESKTOP_CAPTURE_INTERVAL_MS` — much slower
than the ordinary encode cadence — rather than once per encode tick: a
fresh pipe round trip and a GDI capture on a SYSTEM process on every frame
interval would be a needless cost on a privileged process for a picture
that mostly is not changing.

### 6. The indicator

`SessionSnapshot`/`SessionStatusDto` gain `secure_desktop_active`, tracked
the same way `recording_active` already is: a fact about what is happening
right now, separate from `secure_desktop` the permission, read from a flag
the encode loop sets for exactly as long as it is actually serving
secure-desktop pixels for that peer. The host's session list renders it with
the same non-removable, always-rendered-while-true markup the recording dot
already uses — no setting hides it, because §2.2's "no hidden capture"
applies to this more than to anything else this project has shipped.

## Consequences

- `crates/service` grows from one capability to two, and from roughly 120
  lines of dispatch logic to noticeably more, split across a new
  `secure_desktop` module (the desktop switch and the GDI capture) and a new
  `frame` module (the shared mapping) shared with the client half of the
  crate. It remains the only place besides `windows_service.rs`/`install.rs`
  that carries `unsafe` in this crate; `frame.rs` is the fourth `unsafe`
  surface `crates/service` carries (dispatcher, DACL/SCM calls, now the
  mapping), all under the same justification standard ADR 0012 set.
- `crates/service`'s "the client needs no unsafe" property (ADR 0043 §5)
  narrows: opening the pipe and sending two bytes still needs none, but
  reading the frame mapping does, because a memory mapping has no safe std
  wrapper. That code lives in `crates/service::frame`, inside the crate that
  already carries `unsafe` for this exact reason — `apps/desktop/src-tauri`
  itself gains none and stays `#![forbid(unsafe_code)]`.
- A machine with the helper installed and this grant turned on for a
  session now has, for the duration of that session, a `Global`-namespace
  shared memory mapping that can hold a picture of the secure desktop.
  Read access is `IU`-wide by the DACL alone; the session-binding check in
  §3 is what actually narrows who can *cause* something to be written into
  it, not the DACL by itself. This is a real, new addition to what a
  compromised low-privilege process on the same machine could go looking
  for, and the honest answer is that the mitigation is the narrowness and
  the session check above, not an argument that the surface is zero.
- `PROTOCOL_MINOR` (`crates/core/src/protocol.rs`) does not move. Nothing
  about this feature is a guest/host wire message: the grant is enforced
  host-side exactly like the other four independent grants already are, and
  the guest either receives ordinary media frames or the existing
  `MediaUnavailable(SecureDesktopActive)` (`docs/bugs/11-uac-degradation.md`,
  itself already on the wire) — never anything new to decode.
- `docs/bugs/11-uac-degradation.md`'s behaviour is the fallback for every
  failure mode this feature can have — service absent, grant off, session
  check refused, capture failed, mapping unreadable — by construction,
  because the new branch only ever *adds* an attempt before the existing
  arm's own logic runs unmodified.

## Verification

Covered by real tests, run in this environment:

- `Grants`/`IndependentGrant`/`SessionManager` (`crates/core`): the new
  grant is exercised by the same generic property and unit tests every
  other independent grant already has — not derivable from any role,
  deny by default, revoked the instant `set_grant(..., false)` runs, gone
  after `revoke` even once re-granted. See `ALL_INDEPENDENT` in
  `crates/core/src/session.rs`.
- `crates/service/src/protocol.rs`: the new opcode round-trips, is distinct
  from `OP_DELIVER_SAS`, and the frame layout constants agree with
  themselves at compile time (`SECURE_DESKTOP_FRAME_MAPPING_BYTES` is
  asserted equal to header-plus-capacity).
- `crates/service`'s session-binding comparison and the shared-mapping
  read/write round trip are exercised directly (not through a running
  Windows service) on this development machine, which is a real Windows
  host — see the module-level tests in `frame.rs`/`windows_service.rs` for
  what that covers and does not.

**Not verified, and not attempted, per this task's own instruction not to
trigger a real secure-desktop transition in an automated test:**

- Installing `lumepeer-service` end to end and driving a real UAC prompt or
  lock screen through it to a connected guest. That needs an administrator
  prompt this environment cannot click through and, per ADR 0043's own
  precedent for `OP_DELIVER_SAS`, is exactly the kind of action that does
  not belong in a test any contributor might run unattended.
- Whether `OpenDesktopW(L"Winlogon")` actually succeeds for a service
  running as `LocalSystem` in session 0 against a real active secure
  desktop, and whether the resulting GDI capture is a correct picture of
  it. The window-station/desktop switch and the BitBlt compile and run
  their error paths cleanly against the *ordinary* desktop in this
  environment (there is no secure desktop active to switch to), which
  confirms the mechanism does not crash or leak handles, not that it
  reaches the object D8 is actually about.
- The manual Windows checklist in `docs/bugs/15-secure-desktop-capture.md`'s
  Definition of done: install/no-install × grant/no-grant, by hand, on a
  machine with a real UAC prompt.
