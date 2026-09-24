# ADR 0113 — The obfuscated transport meets its host through the DHT

Status: accepted
Date: 2026-09-24

Extends [ADR 0052](0052-serverless-transport-obfuscated-quic-with-stun-discovery.md),
[ADR 0053](0053-invite-ticket-carries-a-stun-address-and-pinned-cert-fingerprint.md) and
[ADR 0111](0111-the-obfuscated-transport-is-an-automatic-default.md).
Settles what [ADR 0082](0082-hole-punching-without-a-signalling-channel.md) left open: the
host's half of the punch.

## Context

On 2026-09-24 both machines of the test pair auto-updated to v0.0.90 and
restarted. After that the guest (Spain) dialed the host (`beta`, KBR) for six
minutes and never got a session. Three separate failures added up.

**The relay cannot carry anything for `beta`.** All four n0 relays are
Hetzner (AS24940, AS213230). From `beta`, every TCP flow to them freezes for
good after **exactly 4698 bytes** of response. That is measured with one TLS
connection making keep-alive `GET /` requests, and it does not depend on the
pace: requests back to back, one a second or one every three seconds all stop
at the same byte. It happens on euc1-1 and on use1-1. The same flow from the
other machine runs 100 requests without a stall, and Cloudflare hands `beta`
2 MB without trouble. Inside iroh this means about 1 KB of application data per
relay connection, then `Ping timeout`, a reconnect, and the same again. A QUIC
handshake gets through; the control handshake behind it does not. So the relay
is useless to `beta` both as a data path and as the channel iroh uses to
coordinate a hole punch.

**The iroh path dialed a dead port.** The endpoint binds a random UDP port on
every run. The DHT went on serving the previous run's record, so every direct
address the guest tried named the old port. `n0_mainline`'s
`get_mutable_most_recent` returns the *first* answer, not the one with the
highest sequence number. A fresh query got the new record once in three rounds.

**The obfuscated transport could not work either:**

- A restored invite (ADR 0062) came back **without** the obfuscated endpoint
  its ticket advertises. The socket had died with the previous run.
- The ticket's address is the host's public mapping at issue time. `beta`'s
  public IP had moved from 176.208.57.14 to 85.173.133.255 within hours, and
  85.173.126.211 had been seen before; it has two uplinks.
- The host never sends towards the guest (ADR 0082), and `beta`'s NAT filters
  by sender. Both NATs are endpoint-independent: the same mapped port towards
  Cloudflare and Twilio, even through `beta`'s double NAT. So a punch can land,
  but only if both sides send.

## Decision

The obfuscated transport gets a rendezvous over the public Mainline DHT
(`crates/net/src/rendezvous.rs`). That is UDP, run by no server of ours, and
iroh already uses it for address lookup. There are two BEP 44 records, both
sealed with `XChaCha20-Poly1305` under a key derived from the invite id, so only
a holder of the invite can read either one:

- **Host record.** It says where the host's obfuscated endpoint is reachable
  now. It is signed with the host's endpoint key under an invite-derived salt,
  so only the host can move it. The host publishes it when the endpoint binds,
  again whenever its keep-alive STUN answer shows the mapping has moved, and
  every 30 minutes so DHT nodes keep it.
- **Knock.** It says where a guest is dialing from: the STUN-reflexive address
  of the very socket that dials. It is signed with a key every holder of the
  invite can derive, because the host cannot know its guests in advance. The
  host polls for it 10 s after its previous poll finished. Each new knock gets
  10 packets of random bytes towards the guest, one a second, from the host's
  own obfuscated socket. That send is what makes a NAT that filters by sender
  let the guest's next packet in.

Every read drains the whole lookup and keeps the highest sequence number. It
never takes the first answer.

What goes with it:

- **The host binds the endpoint again for a restored invite** at startup, and
  keeps it only while that invite is still the live one.
- **STUN answers reach the keep-alive through a tap in `ObfuscatedSocket`.**
  Once `noq` owns a socket it is non-blocking and `noq` reads every datagram,
  so the keep-alive cannot wait for its own answer. A Binding
  success from a known reflector that fails to open as a sealed datagram is
  handed to the tap instead of being dropped. This is how a host notices that
  its uplink moved.
- **The guest reads the host's current address before each control-channel
  attempt, and knocks.** Both run in the background while the punch train
  starts. The train reads its target on every packet, so a lookup that lands
  mid-train redirects the rest of it.
- **The pinned certificate needs nothing new.** It is the same on every bind:
  `rcgen` derives the serial from the key and uses fixed validity dates, and an
  ed25519 signature is deterministic. A test now holds that, because a restored
  invite depends on it. The old comment claiming a fresh certificate per bind
  was wrong.

## Measured

`obfuscated_wan_probe` ran from the dev box (Spain, 176.85.197.66) to `beta`
(double NAT, 85.173.133.255) on public addresses only, with no relay and no
tailnet: **4 of 5 punches connected, in 10–13 s each.** The one that missed
was knocked just after the host's poll, so the host's packets came only near
the end of that attempt. In the app, the next obfuscated attempt of the same
dial round covers that.

The probe had never landed a punch before this, for a reason of its own: it
bound the endpoint under one invite id, while `InviteTicket::issue` minted
another, so the guest sealed with keys the host could not open. It now uses
`issue_with_id`, as the app always did.

## Consequences

- **The polling cost.** On the public DHT one knock poll takes 7–10 s and
  sends about 66 requests for about 20 answers, roughly 15 KB. A host with an
  invite open therefore costs about 0.8 KB/s, some 70 MB a day. The pause
  (`RENDEZVOUS_POLL_SECS`) is the knob: a longer pause is cheaper, but a guest
  may then need a second dial round before the host answers.
- **A forged knock** can make a host send ten small packets to an address of
  the forger's choosing. It cannot let anyone in: the handshake, the invite
  and consent still decide that (§2.3). If two guests knock on one invite
  between two polls, only the newer knock is answered. The other guest's next
  attempt knocks again and is answered at the following poll.
- **No clocks have to agree.** A knock that is new since the last poll is
  answered whatever its timestamp. Only a knock already present when the
  endpoint starts is checked for age (`RENDEZVOUS_KNOCK_FRESH_SECS`).
- **Mixed versions still work.** An older guest never knocks and dials the
  ticket's address as before. A newer guest dialing an older host finds no
  record, gets no punch-back, and behaves as it did before.
- **Out of scope:**
  - iroh's own DHT address lookup still takes the first answer, which is why
    the iroh path dialed a dead port. That needs a lookup of our own, or an
    upstream fix.
  - A random port per run is untouched.
  - A symmetric NAT on either side still defeats the punch.
  - For `beta`, the relay remains unusable while its ISP freezes Hetzner.
