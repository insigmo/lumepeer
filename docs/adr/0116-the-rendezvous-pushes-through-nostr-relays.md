# ADR 0116 — The rendezvous pushes through Nostr relays

Status: accepted
Date: 2026-09-26

Extends [ADR 0113](0113-the-obfuscated-transport-meets-its-host-through-the-dht.md).
Amends the dial order of [ADR 0083](0083-one-dial-plan-per-connect-and-a-fallback-before-consent.md).

## Context

On 2026-09-26 the guest (Spain) connected to `beta` (Rostelecom, KBR) with
v0.0.95 on both ends. The connect took 69 s, the picture came, and 20 s later
the session was gone; the resume took another three minutes. Both logs
together say why:

- **The obfuscated transport never got its punch.** The guest knocked on the
  DHT at every attempt; the host never logged a knock. Its poll pause,
  `RENDEZVOUS_POLL_SECS`, had shipped as `25 * 60` — twenty-five minutes, not
  the ten seconds ADR 0113 measured with, presumably because a ten-second poll
  costs a host about 70 MB a day. Either number is wrong for the job: a poll
  a host can afford answers a knock long after the dial gave up.
- **So the session came up over iroh, on the tailnet.** `beta`'s iroh record
  has no public address (its net report cannot finish while the ISP freezes
  the relay flow), only private ones and its Tailscale address, and that is
  the one that answered. When `beta` dropped off the tailnet, as it does
  several times an hour, iroh fell back to the n0 relay, which the ISP freezes
  after a few kilobytes: `LastOpenPath`, then the media stream, then the
  session.
- **Reconnecting put iroh first again** (`remembered = Iroh`, ADR 0083), two
  attempts of up to twenty seconds each, before the obfuscated transport —
  which could not punch either — got its turn.
- **What took the session down was `beta` itself, not the path.** Lining up
  `beta`'s event log with both app logs: at 13:33:08 Kernel-Power 566 ("the
  system session has transitioned … Reason InputHid"), and from that second
  `beta` reached nothing — not the internet, not its own router 1 ms away —
  until 13:36:43, with no disconnect in the WLAN log; `SessionUnlock` followed
  four seconds later and the session resumed at 13:36:49. It reproduces with no
  lumepeer running: one synthetic Shift press, 2 min 55 s without a network.
  Every input that starts such a transition does it (a host raising its
  consent window through tao's synthetic Alt, a guest's first mouse move), and
  holding `ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED` does not prevent it —
  measured. No transport survives three minutes of a dead uplink; see
  Consequences.

## Decision

**The knock and the host's answer are pushed over public Nostr relays**
(`crates/net/src/nostr.rs`, `rendezvous::Signals`). A host that serves an
invite keeps one WebSocket per relay open with a subscription on a topic
derived from the invite id. A guest publishes its knock there — the same
sealed record as on the DHT — the moment its socket's reflexive address is
known. The host punches towards it at once, asks its STUN reflector again (so
a moved uplink is noticed now, not at the next keep-alive), and says its
current address back on the same topic. The guest moves its dial target there
and **starts a fresh attempt the instant the answer arrives**: the host has
already punched, so a new Initial gets through on its first packet, where the
attempt in flight would wait a second or more for its retransmission.

- Events are of an ephemeral kind (25116): relays forward them to live
  subscribers and store nothing. Content is the sealed record, hex-encoded;
  the Nostr key is random per channel. A relay sees a random topic, random
  bytes and the IP address of whoever connects.
- Eight relays, all of them used at once, measured from `beta` on 2026-09-26.
  Six delivered hundreds of kilobytes on one connection; `relay.damus.io` and
  `nos.lol` freeze after about 15 KB, which is the TSPU throttle on foreign
  hosting (it hit several Cloudflare-fronted relays too), and a signalling
  channel stays far below that between reconnects. Relays that were
  unreachable from `beta` (`relay.nostr.band`, `relay.nostr.net`) are not in
  the list.
- A frozen flow is silent, so every connection pings every 20 s and is
  replaced after 35 s without a frame; a failed connect backs off from 1 s to
  15 s. Idle cost: eight TLS connections and a ping each, a couple of
  megabytes a day — against 70 MB for a ten-second DHT poll.
- The DHT stays: the host record is still published there, and a guest still
  reads it and knocks there, for the day no relay answers.

**Iroh only leads a dial when it last reached the host on a nearby address**
(a private, link-local or loopback one). A host last reached across the
internet — over a relay, or over a tailnet address in 100.64.0.0/10 — is
dialed obfuscated first. On a LAN iroh's direct path is still the near one,
and the obfuscated transport would need the router to hairpin.

**A host retries binding the restored invite's endpoint** on every ping tick
until it has one with a public address. A host that started before its
network came up used to stay reachable over iroh alone for the whole run.

**A dead obfuscated path is given up after 20 s, not 60** (keep-alive every
5 s instead of 15). The idle timeout is how long a guest looks at a frozen
picture before the session notices its path is gone and dials again; with
the dial down to about a second, waiting a minute for a path that is not
coming back is the larger part of "the connection dropped".

**A host is listening again within seconds of its network coming back:** a
signalling relay that failed is retried at most 15 s later, not 60. Together
with the resume dial every few seconds, that is what bounds how long after an
outage like `beta`'s the session is back.

## Measured

`obfuscated_wan_probe` from the dev box (Spain, 176.85.197.66) to `beta`
(double NAT, 85.173.133.45), public addresses only, no relay, no tailnet:

| rendezvous | punches landed | time to connect |
|---|---|---|
| DHT, poll every 25 min (v0.0.95) | never answered in time | — |
| DHT, poll every 10 s (ADR 0113) | 4 / 5 | 10–13 s |
| Nostr push | 10 / 10 | ~1–2.5 s |
| Nostr push + fresh attempt on the answer | 20 / 20 | 0.49–0.84 s, median 0.6 s |

The host hears a knock 0.2–0.4 s after the guest sends it. All eight relays
subscribed from `beta` within one second.

## Consequences

- Third parties now see that two addresses talk to Nostr relays at the same
  moment. They learn nothing of the content, and the same was already true of
  the DHT.
- A relay can drop or delay events, and one may one day refuse the kind.
  Eight independent ones and the DHT behind them cover that; the list is a
  constant to revise when measurements change.
- An attempt cut short by the answer sends a `CONNECTION_CLOSE` Initial. The
  host's accept loop recognises that close (`APPLICATION_ERROR` during the
  handshake) and skips it quietly instead of warning once per connect.
- `RENDEZVOUS_POLL_SECS` is left as it shipped. With the push in place the DHT
  knock is only a fallback, and its cost is what decided its pace.
- `beta` still loses its whole network for three minutes on those input
  transitions, and every session with it drops and resumes across them. That
  is the machine, and no software on the far side can carry a session through
  it: a whitebox with a Realtek 8852BE Wi-Fi (driver 6001.15.162.1, installed by
  IObit Driver Booster 13, whose "Driver Booster Power Plan" is the active
  scheme), "Allow the computer to turn off this device" and "Multi-Channel
  Concurrent" on. Those are where to look; its wired port is unused.
- Both ends need this version: an older host never hears a push knock, and an
  older guest never sends one. Either way the dial behaves as it did before.
