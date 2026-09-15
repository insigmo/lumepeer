# ADR 0087 — Assembling the session-0 host

Status: accepted
Date: 2026-09-15

Builds what [ADR 0085](0085-the-host-can-be-a-service-and-the-screen-belongs-to-an-agent.md)
authorized and did not itself perform. Its own closing section named the gap
precisely — "there is no binary yet that takes the host role, builds
`ActorStores` from the machine store, binds the endpoint and drives an agent —
nor the agent mode of the desktop application that would attach to it" — and
this decision is about the choices that assembling those two forced, none of
which ADR 0085 had to make.

Extends ADR 0085 throughout. Nothing here widens what a guest may do; there is
no new `IndependentGrant` and no new peer-facing message, for ADR 0085's own
reason: the same roles carry the same flags against the same `SessionManager`,
and what changes is which process owns it.

## Context

ADR 0085 decided the shape. Six questions only appear when you write the
binary, and each of them has a wrong answer that would look reasonable in a
diff.

## Decisions

### 1. The host is its own crate, and the two services do not share their installer

`crates/host` is a new binary crate. It could not be a second mode of
`crates/service`, because ADR 0049 §2 makes that crate's dependency list part
of its security argument — it names no lumepeer crate at all, and a reviewer
can check that in one glance at a `Cargo.toml`. `LumepeerHost` links the whole
runtime: `lumepeer-core`, `lumepeer-net`, `lumepeer-runtime`, and through them
iroh, sqlx and a media crate. Putting the two in one package would end that
property for the helper as well as for the host.

The same constraint decides the duller question underneath it. Both services
register themselves with the service control manager and both report a
lifecycle to it, and that is roughly two hundred lines of Win32 each. It is
**deliberately duplicated**, and the two alternatives are worse in ways that
are not about line count:

- *Shared through `crates/service`'s library.* That library is what the
  unprivileged desktop client links, and its header says nothing privileged
  lives there. `CreateServiceW` is not privileged — it fails without
  administrator rights — but moving the elevated path's code into the crate the
  unelevated client links makes the header's claim something a reader has to
  qualify rather than check.
- *Shared through a third crate both depend on.* That puts a lumepeer
  dependency on `crates/service`, which is the property above.

What the two copies can drift on is a display name, a description and a start
type, all of which are visible in `services.msc`. That is an acceptable class
of drift. The class that is **not** acceptable — two implementations of who may
do what — is the one ADR 0085 §1 already settled by extracting the actor rather
than copying it, and it is untouched here.

`log` moves the other way, into `crates/service`'s library, parameterized by
file name. Two services now start with no stdout for the same reason, and
where the file goes and what bounds it is one question asked twice.

### 2. The machine keystore's secret is random, local, and honest about what it is

`FileKeystore` derives its file key from a caller-supplied secret and asks for
"OS user-specific material". A `LocalSystem` service has no user. The secret is
therefore thirty-two random bytes, minted on first run into
`keystore.secret` **inside the machine store directory** — beside the keystore
it protects.

That is worth stating plainly rather than presenting as layered defence:
anybody who can read the keystore can read the secret. ADR 0085 already said
the protection is the directory's access list, and rejected machine-key DPAPI
on exactly these grounds. What the secret buys is the narrow case it actually
covers — a keystore file carried off on its own, by a backup tool or a support
bundle, is not a device password — and the alternatives are worse: a constant
would be shared by every machine that ever hit this path, and anything derived
from the computer name or the machine GUID is guessable by anything that can
already read the directory.

A secret that reads short is **refused**, not padded. A truncated file is a
crash between create and write, and padding it would silently weaken every key
derived from it afterwards.

### 3. The agent's indicator belongs to the attachment, not to the session count

`ViewWindows::set_host_bar(visible)` is called by the runtime whenever the
number of active sessions changes. On the desktop client that is exactly right:
the bar carries the revoke and belongs up while somebody is connected.

The session-0 host implements it as **nothing**, and the reason is the whole of
ADR 0085 §3b. Forwarding it as `ShowIndicator { on: visible }` would look like
the same thing and would not be: `false` arrives on every drop to zero,
including the gap between one guest leaving and the next arriving, and §3b's
property is that the indicator is up *before* a frame can leave — not that it
is up once one has. A banner that blinks off between sessions is a banner
somebody can be filmed under.

So the indicator follows the attachment: `SessionScreen::on_event` raises it
the moment an agent attaches and it stands until that agent is gone. The cost
is an indicator on a machine nobody is connected to, which is the direction
this project errs in on purpose.

There is deliberately nothing on it to press. A control somebody can dismiss is
a control somebody can be talked into dismissing, and the person at a machine a
service is hosting has no revoke to reach anyway — the controls live in a
`LocalSystem` process that draws nothing.

### 4. A scroll wheel is input, so it is on the agent's wire

`AgentCommand::Wheel { dx, dy }` joins the command set. It was the one
`InputDetail` with no shape on that wire, and a guest whose scroll wheel
silently did nothing on a service host — while keys and clicks worked — is
precisely the quiet degradation §18 forbids.

The layout does not grow: the deltas travel in the pointer slots, reinterpreted
as signed. Seven of the eight commands do not use those two bytes, and a second
pair would be carried by every message so that one kind could use it.

This is the internal service↔agent channel, not the peer protocol. No
`MessageKind`, no `PROTOCOL_MINOR`, no golden vector: nothing a peer can see
changed.

### 5. What crosses the frame mapping is the media wire's payload, not a bitstream

`lumepeer_runtime::view::encode_media_payload` is public, and the agent writes
exactly its bytes into the mapping.

The alternative — the agent writes a bitstream and the host frames it — reads
as tidier and puts a **`LocalSystem` process in the business of parsing
attacker-influenced bytes**. It would also be a second place where a keyframe
flag and a capture timestamp are laid out, kept in step by review. With the
payload built agent-side, the privileged process copies a byte range it never
opens, which is the same relationship it has to the secure-desktop worker's
frame and for the same reason.

### 6. A service host runs on defaults, and asks for monitor zero

`lumepeer_runtime::config` resolves per-user paths, which is exactly what a
`LocalSystem` process must not read — a service that took its relay URL from
whichever profile happened to be first would be configured by whoever signed in
earliest. So the host runs on `Settings::default()`, and making a machine-wide
settings file is not done (below).

The monitor index is `0` and is not settable from the host. The index means
something only inside the agent's own session — it is an offset into what
*that* desktop can see — so a host choosing between monitors would be naming a
thing it cannot enumerate.

## What is built against this decision, and what is not

Stated the way ADR 0085 stated its own shortfall, and for the same reason.

Built, and covered by tests that run anywhere:

- `crates/host`: the host role claim and its refusals, the machine store and
  its paths, the identity, the endpoint, the actor, and the agent supervision
  loop.
- The `ViewWindows` seam for a host with no windows, including the attendance
  answer ADR 0085 §2's refusal is built on.
- The input path: every `InputDetail` crosses to the agent unchanged, and an
  event with no agent attached is refused out loud rather than dropped.
- The agent mode of the desktop application: the channel, the indicator, the
  input, and a capture-and-encode loop that publishes into the mapping.
- `AgentLink::accept_from_while` and the two writing halves, which the pieces
  from pack 25 did not have and could not work without: the accept was an
  unbounded `ConnectNamedPipe`, and one handle cannot be blocked reading and
  ready to write at once.

**Not built: the path that carries the agent's frames out to a guest.** The
host publishes a mapping, the agent fills it and says so, and `SessionScreen`
records the sequence — and there the frames stop. `Actor::on_media_accepted`
still starts `spawn_encode_loop`, which pulls from a capture backend this
binary does not have. Until that is closed, a guest on a service host gets
control and no picture: the honest `MediaUnavailable` of §18, which is the
same state ADR 0085 already describes for a machine with nobody signed in.

It is not closed here because a relay that merely moved bytes would be worse
than none, and the four obstacles are worth writing down rather than
rediscovering:

- **Keyframes.** `MessageKind::KeyframeRequest` reaches an encoder today. The
  encoder is now in the agent and there is no command that reaches it, so a
  guest that joined mid-stream or lost more than it could conceal would never
  recover. A relay without this is a session that goes grey and stays grey.
- **Bitrate.** ABR (§11) calls `set_bitrate` on the encoder. Same problem, less
  severe: the stream would work and would not adapt.
- **The codec is fixed.** The agent encodes with `EncoderConfig::default()`,
  and `StartCapture` carries a monitor and nothing else. A service host is
  therefore H.264-only, and `choose_media_codec`'s negotiation has nothing to
  negotiate with. Either the command grows a codec or the host stops offering
  the choice; deciding which is part of the same piece of work.
- **`MediaHealth` cannot become healthy.** It has `record`, which sets a fault,
  and no way to clear one. A service host's ability to produce a picture
  arrives when an agent attaches and leaves when it dies, and the §18 check in
  `on_media_accepted` — which is what stops a host accepting a media connection
  it will never write a frame on — reads that struct.

Also not done, and smaller: a machine-wide settings file (decision 6), carrying
a guest's monitor choice to the agent, and audio — the agent captures no sound,
so a service host is silent.

## Verification

What is provable without a second machine, and is: everything in the "built"
list above, as unit tests in `crates/host`, `crates/service` and
`crates/runtime`.

What needs hardware, and is therefore a `docs/release-checklist.md` step rather
than a test any contributor runs — **none of which has been run**, because the
one machine this was written on runs a helper service that must stay as it is:

- Installing `LumepeerHost` beside `LumepeerHelper` and both staying up.
- A guest connecting to a machine at its logon screen, being refused with no
  unattended credentials configured, and admitted with them.
- Signing in and seeing the indicator appear without being asked for it.
- Signing out killing the agent, and the guest seeing an honest state rather
  than a frozen frame.
- The desktop client refusing to become a second host while the service holds
  the token, and the handover working in the direction ADR 0085 §4 chose.
- The access list on the machine store directory, read back on a real install.
