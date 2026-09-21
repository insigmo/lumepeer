# ADR 0101 — A terminal session is the same session with no media connection

Status: accepted
Date: 2026-09-21

Follows [ADR 0079](0079-a-terminal-is-its-own-grant-and-never-the-clients-privileges.md),
which built the terminal as a capability *inside* a remote-view session, and
answers the one question it did not have to: what a guest gets when the shell
is the only thing it came for.

## Context

A saved host's card offers "Connect again". What comes back is a window with
that machine's screen in it, and a terminal panel the guest can open on top.
For the case the terminal was written for — run a command on a machine, read
what it said, leave — everything around the shell is cost: a capture on the
far side, an encoder, a stream, a decoder, and a picture nobody looks at.

`ssh host` is the shape people already have for this. The question is what it
costs to offer it here.

Three things are true of the code as it stands, and they are what this
decision rests on:

- **The host's encode loop starts when it accepts `rd/media/1`, and at no
  other moment** (`Actor::on_media_accepted`, "media connection accepted;
  starting the encode loop"). A guest that never dials that connection is a
  host that never encodes and never writes a frame.
- **Only the encode loop reads a frame.** Outside its own unit tests,
  `CaptureController::next_frame` has exactly one caller, inside
  `spawn_encode_loop`.
- **`terminal` rides `Role::FullControl` alone** (`Grants::from_role`), and
  the role is fixed by the invite code the host handed out. The guest does not
  choose it.

## Decision

### 1. It is the same session, the same role and the same window label

No new `Role` variant — the enum travels in `Hello` and `ConsentGrant` through
postcard, and a variant appended for this would be a wire change that older
peers cannot parse for a feature that needs nothing on the wire. No new grant:
the shell is `terminal`, which ADR 0079 already defined, re-read by the host at
every `TerminalOpenRequest`. No new ALPN, no new message, no
`PROTOCOL_MINOR` bump. The window is still `view-{peer}`, so
`check_view_window` keeps letting exactly that window's `terminal_*` commands
through and nothing else.

What a guest asking for a terminal changes is **one thing**: it does not dial
`rd/media/1`. Everything the guest gets, and everything the host does not do,
follows from that single omission — and the one thing it does not buy is in
the consequences below, said out loud rather than glossed.

### 2. The intent lives on the guest, from the dial to the window

It has to survive the handshake, which is several actor turns long, so it is
recorded where the dial starts and read where the window opens:

`HistoryConnectArgs { peer, terminal_only }` → `ActorCommand::HistoryConnect`
→ `Actor::spawn_dial_as` writes the peer into `pending_terminal_only` →
`Actor::start_view` takes it out and, with it set, builds the `ViewState` with
`task: None` — no media receiver, no receive task, nothing to dial with.

It is cleared, not merely consumed: `stop_view` and a connect that settles on
anything other than `Connected` both drop it, so an ordinary Connect to a host
somebody opened a shell on an hour ago is an ordinary session. It is *carried*
in the two places where the same connect continues — ADR 0096's further rounds
and ADR 0089's resume, which reads it back off `ReconnectWait` because
`stop_view` has already run by then. A session that came back as a screen one
would start a capture nobody asked for, which is the outcome this whole
decision is about.

### 3. The window is armed for no input, whatever the role granted

`ViewWindows::open` takes `terminal_only`, and for such a window `input` is
`false` even under full control. Not because the session may not send input —
it may, and `Actor::on_input` still reads the session's own grant — but
because `input: true` is what arms the global keyboard hook of
[ADR 0090](0090-the-keys-a-view-window-never-sees.md). That hook takes `Win+D`,
`Alt+Tab` and the rest away from the person sitting at *this* machine so they
reach the remote screen. A window with no remote screen in it has nothing to
send them to, and taking somebody's own chords away for it would be a cost
with no purchase behind it.

### 4. The window shows what a shell session has, and nothing else

`view.html?…&terminal=1`. The page skips both frame loops, the cursor poll,
the thumbnail capture, the pan and zoom and the status overlay — which would
otherwise sit at "waiting for the host's picture" for the life of a window
where no picture is coming — hides the canvas, and mounts the terminal across
the whole window. The toolbar keeps chat, the file manager (by its own grant)
and the terminal, and drops the settings and monitor popovers, the zoom, the
microphone, Ctrl+Alt+Del, the recording request and full screen: every one of
them acts on a picture or on the media connection that carries it. They are
left out rather than disabled — §18's rule that a control which cannot work is
worse than no control.

Closing the window still ends the session, exactly as it does for a screen
window.

### 5. The card offers it only where it could work

"Connect to terminal" sits in the saved host's `⋮` menu beside "Connect
again", and is disabled for any saved host whose role is not full control,
with a title that says why. The grant comes with the role, the role comes with
the invite code, and the guest cannot ask for a different one — so an enabled
button on a view-only row would be a button whose only possible answer is
`TerminalRefusal::NotGranted`.

## Consequences

- A guest can work on a machine's command line without that machine capturing,
  encoding or transmitting a single frame, over the session and the grant it
  already had. Nothing was added to the wire to make that true.
- A host with no capture backend at all — a server, a Wayland desktop with no
  portal, a machine whose encoder probe fails — is fully usable for a terminal
  session. That already follows from the code: `add_viewer` failing is a
  warning, not a refused session.
- **The host still registers a viewer at the grant.** This is the part worth
  writing down plainly, because it is where the shape of this decision shows.
  `start_granted_session` calls `CaptureController::add_viewer` for any session
  holding `view`, which starts the platform capturer; the guest's intent never
  reaches the wire, and it cannot, without a protocol message this decision
  refuses to add. So on such a host the capture backend is *started* and then
  never read: no encoder is created, no frame is pulled, nothing is written to
  any stream. On a platform where starting the capturer is itself visible — a
  Wayland portal picker, macOS's screen-recording prompt — that prompt still
  appears for a session that will never use it. Moving `add_viewer` to the
  media accept would fix it for every session at the cost of a round trip, and
  would reverse a deliberate behaviour several tests are written against; it is
  its own decision, not a side effect of this one.
- The terminal's own indicator on the host is unchanged and still
  non-removable (ADR 0079 §3): `terminal_active` is a fact about running
  shells, and a session with no picture in it is exactly the one where that
  indicator is the only thing on screen saying somebody is there.
- Two windows onto the same host cannot both exist — the label is
  `view-{peer}` and a second dial to a connected host is refused — so "a
  terminal beside the screen" is the existing terminal panel, not a second
  connect.

## Alternatives considered

- **A `Role::TerminalOnly`.** It is the natural-looking answer and it is a wire
  change: `Role` rides `Hello` and `ConsentGrant` through postcard, and an
  appended variant is a parse failure on every peer built before it, for a
  distinction the host does not need to know about. The host's decision is
  "may this guest have a shell", which `terminal` already answers.
- **A `terminal_only` flag on the wire, so the host skips `add_viewer`.** It
  buys the last consequence above at the price of a protocol message, a
  `FEATURE_*` string and a `PROTOCOL_MINOR` bump — and it would be a flag the
  host *trusts*, which is a shape this project avoids: a guest that lied would
  simply get a picture it asked not to get. The honest version is the one
  above, where the guest's restraint is the mechanism and not a request.
- **A separate window label, `term-{peer}`.** It would mean widening
  `check_view_window`, which is the narrow rule that stops one session's window
  polling another's. A window that is already allowed exactly the `terminal_*`
  commands is the right window.
- **Let the guest open a terminal without a session at all.** That is SSH, and
  ADR 0079 already said what this is not: there is no way in here that does not
  begin with a granted Lumepeer session.
- **Drop the media connection mid-session instead, so one window could switch
  between screen and shell.** The host's encode loop is per media session and
  its teardown is `stop_media`, which is also how a revoke ends capture; making
  it a thing a guest toggles would put a guest's press on the same path a
  revoke takes. A connect is cheap; that path is not one to share.
