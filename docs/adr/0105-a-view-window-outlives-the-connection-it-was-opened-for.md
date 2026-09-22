# ADR 0105 — A view window outlives the connection it was opened for

Status: accepted
Date: 2026-09-22

Answers `docs/bugs/22-connection-survives-the-network.md` defect A, which is
the visible half of "after the VPN comes up the connection drops". Narrows
[ADR 0089](0089-a-dropped-session-resumes-inside-its-window.md), which
gave a dropped session a way back, by one thing it left out: where the picture
goes while it is coming back.

## Context

[ADR 0089](0089-a-dropped-session-resumes-inside-its-window.md) and
[ADR 0091](0091-a-resume-is-paced-like-a-resume.md) already do the hard
part. A guest whose link goes away claims its session back for
`RECONNECT_WINDOW_SECS`, every `RESUME_RETRY_SECS`, and the host hands the
role and the grants back without asking anybody. On this pair that works, and
it takes about twelve seconds.

What the user sees in those twelve seconds is a window disappearing and a
different window appearing. `Actor::stop_view` ran on the drop — it is the one
funnel every ending goes through — and it closes the window before anything
has decided whether this ending is an ending at all. So a resume that *works*
is indistinguishable, on screen, from a session that died: window gone,
picture gone, and a new window in a new place a moment later.

"Соединение отваливается" is a description of that, and it stays true however
well the resume underneath it works.

Everything needed to do better is already built. `ViewStatus::Reconnecting`
exists and `view-window.ts::viewOverlay` already renders it as a *non-blocking*
banner over whatever picture is on the canvas — it was written for the media
pipeline's own recovery pass, which keeps the window and the last frame when
only the media connection drops. The control connection dropping simply never
reached it.

## Decision

### 1. A drop that a wait will answer parks the window instead of closing it

`Actor::end_view` takes a `ViewEnd`, and the two callers name the two cases:

- `stop_view` — `ViewEnd::Closed`, everything it did before. The host revoked,
  the user closed the window, the session ended.
- `park_view` — `ViewEnd::Parked`. The link went away and `on_disconnect` is
  about to arm a `ReconnectWait`, so the window stays up.

`on_disconnect` picks between them with the predicate it was already computing
for the wait: a session that *was* watching, and either a resume claim to make
or a host it may dial unasked. Anything else still closes.

The media task is aborted either way, and everything else that rode the
connection — the tunnels, the shells, the transfers, the clipboard watch — is
torn down by `on_disconnect` exactly as before.

### 2. What a parked window keeps, and what it loses

It keeps its `ViewFeed`. That is the whole mechanism: the window has been
polling that feed since it opened and goes on polling it, so it never learns
the session went away as anything but a change of status — and it learns the
session is back the same way, by the picture moving again. `view_frame` and
`view_chunk` read only the feed, so neither starts failing and neither loop
stops.

It loses, at the moment of parking:

- **input** — `ViewFeed::input` is set `false`, so the window stops accepting
  keystrokes on its very next poll. A chord typed at a frozen picture has
  nowhere to go, and §8.1's rule is that the feed carries the live grant.
- **the recording indicator** — the host is no longer saying anything about
  recording, and §17's indicator may not keep claiming it is.
- **the decoder's footing** — `BitstreamFeed::desync`, so the window throws its
  decoder state away and waits for an intra frame rather than painting garbage
  over the picture it is keeping ([ADR 0058](0058-the-guest-decodes-the-bitstream-in-its-own-webview.md)).
- **its media connection handle** — a mic toggle must not find a handle to a
  closed connection.

The status becomes `ViewStatus::Reconnecting`, which keeps the frame and puts
the banner over it.

### 3. The window lives exactly as long as the wait

A parked window is a window something is still trying to fill. That is one
predicate, and it is enforced in one place: `stop_reconnect_wait` — the funnel
every *giving up* already went through — now closes the parked window too.
`finish_reconnect_wait` is the same bookkeeping without that, for the two
places where a wait ends because it **worked**: the `ConsentGrant` that is
about to be drawn into the window, and the handshake of a wait's own dial that
is about to produce one.

So the three endings the bug document asked for fall out of the existing code
rather than being listed again: the host refusing (`on_resume_refused` with no
further wait), the window running out without a continuation, and the user
calling it off (`on_connect_cancel`, or closing the window, which `on_revoke`
now recognizes as a parked one). `close_parked_view_without_a_wait`, at the end
of `on_disconnect`, catches the one case that is nobody's funnel: a host that
answered a wait's dial, ended it, and then dropped without granting anything.

### 4. A session that comes back comes back into that window

`start_view` checks for a parked view before it opens anything.
`revive_parked_view` puts the `ViewState` back into `self.views` with the role
and grants of the new `ConsentGrant`, restores `input`, and spawns a media
receiver against **the feed's own bitstream** — the queue the window has been
polling all along, which is waiting for exactly the intra frame the re-dialed
media connection opens with. `ViewWindows::open` is not called, so there is no
second window, no new label, no re-raise and no re-layout.

A terminal-only session has no media task to move its status off
`Reconnecting`, so reviving one sets the slot back to `Waiting`
([ADR 0101](0101-a-terminal-session-is-the-same-session-with-no-media-connection.md)).

## Consequences

- A resume that works is, on screen, a picture that stopped for a few seconds
  under a banner and then carried on. This is what the complaint was about and
  it is fixed whether or not anything else in that document is.
- **ADR 0089's line is not moved.** "What lives on the connection does not
  survive it" still holds for everything that lives on the connection: media,
  tunnels, shells, transfers, chat, clipboard, staging. What survives is the
  window and the last picture drawn on it, and neither of those is session
  state — the window is a place to put a picture, and the picture is one the
  user was already looking at.
- **The grant boundary is not moved either.** The window coming back is not a
  session coming back; only a `ConsentGrant` does that, under §10's rules and
  inside §10's window. A parked window has `input` off and no connection
  behind it, so it can do nothing at all.
- `view-{peer}` can now outlive the session it was opened for, so the label
  could in principle be held against a *new* session to the same peer. It
  cannot collide: the parked window's peer is registered in
  `rebuild_labels_and_snapshot` like a live one, this node makes one outgoing
  attempt at a time, and a second connect to that peer either reaches the same
  window through `revive_parked_view` or takes the wait down (and the window
  with it) through `stop_reconnect_wait` before opening anything.
- One test changed its mind, deliberately:
  `a_dropped_link_resumes_the_session_with_its_grants_and_asks_nobody` used to
  assert the guest's window *opened a second time*. It now asserts one `open`
  and no `close` at all, which is the stronger statement.

## Alternatives considered

- **Leave the window and let the frontend decide.** The webview cannot: a
  parked window's only signal is the feed, and the runtime is what knows
  whether a wait was armed. Putting the decision in the page would mean a new
  IPC command answering a question the actor already has the answer to.
- **Keep the `ViewState` in `self.views` and mark it parked.** `self.views`
  is read all over the actor as "sessions this node is watching" — the
  clipboard watcher, the snapshot, every IPC command's lookup. A parked entry
  would make every one of those sites ask a second question, and a site that
  forgot to would act on a session that is not there.
- **Keep the window across ADR 0084's wait only if the host is trusted.**
  That is the shape `may_auto_reconnect` already has, and it would tie what
  the user *sees* to a flag about what this node may *dial*. The window is the
  user's, not the dial's.
- **Close the window when the resume window elapses, even though the wait goes
  on.** It draws a line the user cannot see — `RECONNECT_WINDOW_SECS` is about
  whether grants come back unasked, which is not a question about a window —
  and it would close the picture at the exact moment the wait is still working.
