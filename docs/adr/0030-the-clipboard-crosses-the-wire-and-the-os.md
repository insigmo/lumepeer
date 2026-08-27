# ADR 0030 — The clipboard reaches the OS, and each direction is one grant

Status: accepted
Date: 2026-08-26
Extends: §9.2 (clipboard sync), §8.2 (roles and grants), §2.3 (the webview
authorizes nothing), §15 (what is never recorded)
Builds on: ADR 0023 (`ClipboardSync` on the control channel), ADR 0029 (the
four independent grants became issuable), ADR 0027 (blocking work leaves the
actor loop)

## Context

`ClipboardSync` has been in the protocol since ADR 0023, `clipboard.rs`
validated payloads against §9.2 and suppressed the echo loop, and the actor
sent and received the message. Two things were missing, and one thing was
wrong.

Missing, first: **nothing in the tree ever read or wrote a real clipboard.**
`cargo tree` had no clipboard dependency at all. A payload arriving from a
peer was staged in a map for the UI to pull and then went nowhere. The feature
was end-to-end except for both ends.

Missing, second: **no UI ever called `clipboard_push`.** The IPC command
existed, the capability allowed it, and `grep -rn clipboard_push
apps/desktop/src` was empty.

Wrong: **the receive-side grant checks were mirrored.** A host receiving a
guest's clipboard checked `clipboard_read`, and a guest receiving the host's
clipboard checked `clipboard_write` — in both cases the grant belonging to the
other direction. While the four grants of ADR 0029 were unreachable this was
invisible, since every check read `false` anyway. The moment they became
issuable it would have become the worst kind of bug: a host switches on
exactly the permission the UI describes, and watches nothing happen.

## Decision

### The direction-to-grant mapping lives in `crates/core`

`ClipboardFlow` and `permits` (`crates/core/src/clipboard.rs`) state the rule
once: `HostToGuest` is `clipboard_read`, `GuestToHost` is `clipboard_write`.
The send path and the receive path now ask the same function instead of each
spelling the rule out in its own `is_some_and` closure, which is how they came
to disagree. Restating an authorization rule at a second site is how the rule
develops a second opinion.

### The two sides do not ask the same question, on purpose

ADR 0029 settled that grants do not travel on the wire, and that a guest holds
no independent grants of its own. That has a consequence this work had to face
squarely: `Grants::from_role` is the only thing a guest's `ViewState.grants`
is ever built from, so *every* clipboard grant on the guest side reads `false`
and always will. "Make the receiver check the same flag as the sender" would
have produced a clipboard that is structurally impossible in both directions.

So each side checks what it is actually entitled to decide:

- **Host → guest.** The host checks `clipboard_read` before sending. The guest,
  on receipt, checks that the payload came from a host it has an open view
  with, and applies it. The guest is not authorizing anything by accepting data
  addressed to it; the host's core already decided, and §2.3 puts that decision
  nowhere else.
- **Guest → host.** The host checks `clipboard_write` on arrival, and that is
  the decision. The guest's side of it is not a grant but a gesture: the
  payload only leaves the guest from a deliberate press on the view toolbar.

That gesture is the reason the guest has **no clipboard watcher**. A guest that
polled its own clipboard and shipped every change to a host that may not even
be allowed to receive it would be leaking from the *guest's* machine, to
decide something the guest cannot see the answer to. The host user turned a
switch on; the guest user presses a button. Both directions end up gated by a
person, which is what §8.2 is for, and neither needed a new wire message.

### The host clipboard is Rust's, never the webview's

`clipboard_os.rs` wraps `arboard` behind an `OsClipboard` trait. The Tauri
clipboard plugin was the obvious alternative and is rejected: a capability
entry would hand the untrusted presentation layer (§2.3) a standing handle on
the host user's clipboard, live whether or not any session exists and whatever
any grant says. The actor holds the handle; the webview asks the actor.

The one place a webview touches a clipboard is the guest's own toolbar press,
which reads the *guest's* clipboard through the ordinary web API on the
guest's own machine, on the guest's own gesture. Nothing about the host is
reachable that way.

### Everything OS-facing runs on its own thread

Reading a clipboard is not an in-process lookup. On X11 it is a round trip to
whichever client owns the selection, and that client can be slow or wedged.
Doing it on the actor loop would put an unrelated application's
responsiveness in the path of a revoke — the failure ADR 0027 exists to
prevent. So a dedicated thread owns the handle, takes writes as jobs and
reports changes back as events, and the actor never blocks on either.

`arboard` has no cross-platform change notification, so the thread polls at
`CLIPBOARD_POLL_INTERVAL_MS` (500 ms, `constants.rs`). That number is not in
the design doc, because §9.2 assumes a change is observable; it is a
latency-versus-cost trade, not a protocol value, and it is a constant rather
than a literal for the reason §14 gives.

### No grant, no read

The poll runs only while at least one active session holds `clipboard_read`,
and stops when the last one goes. Not "reads and discards" — does not read.
This is §8.1's "no capture without a viewer" applied to the other thing on the
host that is private by default, and it is asserted as a read *count* in the
tests, because an assertion on what was sent would pass for an implementation
that read the clipboard anyway.

Turning a grant on adopts whatever is on the clipboard as a baseline instead
of sending it. The host allowed the guest to see what they copy, not what they
had copied before deciding.

### The echo loop is broken twice

`ClipboardSync::local_changed` already suppressed the per-peer echo. The
thread also remembers what it last wrote, so a payload applied on a peer's
behalf never even reaches the actor as a local change. The second half is what
makes the poll safe: content-diff polling cannot otherwise tell "the peer's
text that I just applied" from "the user copied that text".

One consequence, accepted: a host that applies peer A's clipboard does not
forward it to peer B. Nothing is lost that a user notices, and the alternative
is a host that relays between guests who were each granted a session with the
host and not with each other.

### The UI shows that a clipboard synced, never what synced

`clipboard_pull` stays, and stays a pull: the host panel calls it per active
session and keeps only a timestamp. The returned text is dropped where it
arrives. §15 keeps clipboard content out of the audit log and out of
telemetry, and a panel that is on screen for the whole session is read by
whoever walks past the machine. Nothing renders it, nothing logs it, and no
notification carries it — `ActorNotification::ClipboardFromPeer` is a bare
variant precisely because notifications are broadcast to every listener.

## Consequences

- Clipboard sync works, in both directions, for the first time.
- A new dependency: `arboard` (three crates, `image-data` off — §9.2 is text
  only for v1). `clipboard-win` and `error-code` are BSL-1.0, added to the
  `deny.toml` allow list: OSI-approved, permissive, and with no binary notice
  requirement at all, so strictly less demanding than the MIT already there.
- `spawn_actor_with` grows a third seam next to `windows` and `media`. CI has
  no display; a clipboard that could not be faked would be a clipboard that is
  never tested.
- `capabilities/view.json` gains `allow-clipboard-push` and nothing else. That
  is the narrow half of the pair on purpose: a view window can offer the
  guest's own clipboard on a press, and has no way to read the host's — the
  host's clipboard only ever travels because the host's core decided it may.
- The guest's clipboard leaves the guest only on a press. There is no toast
  telling the guest that the host dropped it for lack of `clipboard_write` —
  the host sends nothing back, and inventing an answer would be the wire
  message ADR 0029 declined. A guest that needs to know asks the host.
- Images and file lists remain out of scope. `OsClipboard` cannot express
  them, which is the cheapest way to keep them out.

## Verification

- `crates/core`: each direction takes its own grant and only its own; a
  `FullControl` role implies neither.
- `clipboard_os.rs`: nothing is read while the watch is off; the first look
  after a grant is a baseline and not a change; an applied payload does not
  come back as a local change; a change made while unwatched is not reported
  when the next grant arrives.
- `network.rs`, two real actors over loopback: a granted session alone never
  reads the host's clipboard (`reads == 0`); `clipboard_read` carries a host
  copy to the guest's own clipboard and withdrawing it stops the flow
  mid-session; `clipboard_write` is what lets a guest change the host's
  clipboard, and holding it starts no read watcher; a revoke stops the
  watcher.
- `tests/integration/tests/consent_cycle.rs`: both directions over a real
  control connection with exactly one grant on.
- `clipboard-ui.test.ts`: the host panel says a clipboard arrived and never
  what was in it, the note ages out, and the guest's button is
  keyboard-reachable, named in both locales, and sends only on a press.
