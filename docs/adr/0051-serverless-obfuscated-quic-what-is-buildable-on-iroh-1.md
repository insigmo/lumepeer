# ADR 0051 — Serverless obfuscated QUIC: what is buildable on iroh 1.0.2

Status: accepted
Date: 2026-09-01

## Context

Task `docs/tasks/17-serverless-obfuscated-quic.md` asks for a host↔guest
session by invite code with **no server of any kind** — no own relay, no VPS,
no CDN, no public n0 relay pool — carried by QUIC-over-UDP, hardened against
Russian-operator DPI that cuts UDP/QUIC by signature. The plan is staged: Fase
0 (measure whether removing the relay already fixes it), Fase 1 (serverless
rendezvous), Fase 2 (an obfuscated datagram layer), Fase 3 (active
countermeasures), Fase 4 (tests). Two of the plan's premises did not survive
contact with the running system, and the difference is large enough to record
here rather than absorb silently.

### Fase 0 measured a wall the plan half-anticipated

Measured on the two-machine rig (`[[project-lumepeer-windows-e2e-rig]]`) — this
machine (guest, Spain) and `beta` (host, Rostelecom, Nalchik), over the
providers, not Tailscale — with `tailscale netcheck` on both sides and
`tailscale ping`:

- Both peers have a public STUN-reflexive address and a **non-symmetric** NAT
  (`MappingVariesByDestIP: false`), so the NATs are punchable in principle —
  `tailscale ping` flapped from relay-only to a **direct** hit on
  `94.25.111.75:8203` within minutes. But the reflexive **port moves per
  socket** (`9476` in netcheck vs `8203` in the ping), and Tailscale only
  punches it because it has a live reflector (DERP/STUN) and a signalling
  channel to learn the current `ip:port` and time the punch. A static invite
  code, handed over once, cannot carry a port that changes per socket.
- `beta` sits behind a **double NAT**: NAT-PMP on its Keenetic router reports
  the router's WAN address as `192.168.100.2` (RFC1918), while its real public
  address `94.25.111.75` is visible only via STUN. A UPnP/NAT-PMP/PCP mapping
  opened on `beta`'s own router is therefore stranded behind the ISP's outer
  NAT and unreachable from the internet.
- `portmapper` (UPnP/NAT-PMP/PCP, via `igd-next`) is already an **iroh
  dependency** and already runs under the normal bind, and `beta` is still not
  directly reachable.

The load-bearing conclusion: a NAT'd host cannot learn its own live public
reflexive `ip:port` without an external reflector; the router only reveals the
router's WAN address, which for a double-NAT host is private. iroh's relay does
double duty today — fallback transport (TCP+WebSocket) **and**
reflector/rendezvous — and only the transport half is TCP. Removing "the
server" removes the rendezvous half too. So "just turn the relay off" does not
work for this pair; it removes the bootstrap that makes any direct path
possible. This is the CGNAT limitation the plan already flagged as a hard wall,
sharper than expected: it hits `beta`'s ordinary home broadband, not only
mobile CGNAT.

### Fase 2's integration point does not exist in iroh 1.0.2

The plan specifies the obfuscation seam as "a user UDP socket under quinn:
quinn accepts a custom `AsyncUdpSocket`, and encryption lives there." That is
the quinn model. This repository does not use quinn directly: the direct P2P
path is `noq`/`noq-udp` (n0's QUIC fork) behind `iroh::Endpoint`, and iroh owns
its UDP socket privately inside its magicsock (`impl noq::AsyncUdpSocket for
Transport`, a multiplexer over IP, relay and custom transports). `iroh 1.0.2`'s
`endpoint::Builder` exposes `bind_addr`, `transport_config`, `external_addr`,
`clear_ip_transports`/`clear_relay_transports` and `add_custom_transport` — but
**no hook to wrap or replace the IP transport's socket**. Three ways to put
obfuscation onto a live iroh session exist, none a drop-in:

1. **iroh custom transport** (`add_custom_transport`, behind the
   `unstable-custom-transports` feature): an obfuscated UDP transport addressed
   by `CustomAddr`. No fork, but an unstable API, and iroh does no STUN/hole
   punching for a custom transport, so it would have to carry its own
   addressing.
2. **Fork iroh** to wrap the IP transport's socket — which the plan itself
   scopes out ("changing the iroh version is a separate PR … §5 and its own
   ADR").
3. **A standalone `noq` endpoint** with a custom `AsyncUdpSocket`
   (`noq::Endpoint::new_with_abstract_socket` does accept one) — a parallel
   transport stack the app would have to select over iroh, duplicating what
   iroh already provides.

All three consume the same thing: an obfuscation **codec** for the datagrams.
None of them is a change to iroh's socket that this task can make in place.

## Decision

Scoped to the direction chosen after Fase 0 (obfuscation is the core; keep
rendezvous minimal):

1. **Fase 1 is reduced to the honest minimum.** No serverless rendezvous
   mechanism is added, because iroh already runs `portmapper` and it does not
   rescue a double-NAT host. What Fase 1 owes is the honest limitation — a
   host behind CGNAT/double-NAT is not serverlessly reachable, and the user
   must be told that plainly rather than left on a timeout — plus the existing
   ticket already carrying the best address the endpoint has, and the manual
   port-forward path for a single-NAT host with a public WAN. The UI string and
   that path are follow-up UI work, not transport code.

2. **Fase 2 lands as a codec, not a socket.** `crates/net/src/obfuscate.rs`
   holds the wire format and its keys and nothing endpoint-shaped:
   - Directional keys derived from the ticket's `invite_id` with
     `blake3::derive_key` under a per-direction context string, so host↔guest
     and guest↔host fail independently and a datagram cannot be replayed the
     other way. The ticket is host-signed, so the key material is authenticated
     before use.
   - Each datagram is `nonce || XChaCha20-Poly1305(len || payload || padding)`:
     a fresh random 24-byte nonce (so two seals of one payload differ), random
     padding up to `OBFUSCATE_PADDING_MAX_BYTES` sealed inside the envelope
     (blurs length, adds no header), and no constant bytes anywhere — uniform
     random on the wire.
   - `open` treats its input as untrusted: it never panics, never indexes
     unchecked, and collapses every failure into one opaque `NetError::
     Obfuscation` so a bad datagram is dropped silently with no decryption
     oracle. This reuses the crate's existing `chacha20poly1305` and `blake3`
     dependencies; **no new dependency is added.**

3. **Everything that needs a live QUIC endpoint is deferred to a §5 follow-up.**
   Wrapping a real socket with this codec, the mandatory `keep_alive_interval`
   on the transport config (without which QUIC's idle timeout looks like a DPI
   drop at ~30 s — `[[project-lumepeer-quic-vs-relay-transport]]`), the
   non-443 default port and port hopping, and the two-QUIC-endpoint integration
   test all belong to whichever wiring path (custom transport or iroh fork) is
   chosen for that follow-up. That choice is deliberately left open here; both
   reuse this codec unchanged. Fase 3's active countermeasures (low-TTL decoy,
   per-attempt port change, adaptive escalation) are also endpoint-layer and
   move to the same follow-up.

4. **No existing path is touched.** `PeerEndpoint`, the direct iroh path and
   the relay are unchanged; the codec is added beside them and wired into
   nothing yet. No iroh fork or version bump is made in this task.

## Consequences

- The product can tell a user behind CGNAT/double-NAT the truth — "your
  provider does not give you a direct address" — instead of a silent failure,
  once the Fase 1 UI string lands. It cannot make such a host reachable without
  a reflector, and a reflector is a server; that is a property of the network,
  not a gap in this code.
- The obfuscation codec exists, is unit-tested (deterministic direction-keyed
  derivation; seal/open round-trip both ways; nonce variance; silent rejection
  of wrong-key, tampered and short input with no panic) and is proven over a
  real UDP socket pair. Its DPI-survival value is only realized once the §5
  follow-up wires it into a live path, and that can be validated only where a
  path exists — one LAN, a manual port-forward, IPv6, or a single-NAT peer —
  not on this pair's provider path, which Fase 0 showed is serverlessly
  unreachable.
- The plan (`docs/tasks/17-serverless-obfuscated-quic.md`) is updated to match:
  Fase 0's result is recorded there, Fase 1 is marked reduced, and Fase 2/3's
  endpoint-level items are marked as the §5 follow-up. The task does not claim
  a working serverless DPI-resistant session on iroh 1.0.2, because it cannot
  deliver one without the endpoint-integration step.

## Verification

- `cargo test -p lumepeer-net obfuscate::` — the codec's unit and real-socket
  tests pass.
- `cargo clippy -p lumepeer-net --all-targets -- -D warnings` — clean under the
  workspace's pedantic lints.
- Fase 0's network measurements are in the session scratchpad
  (`fase0-net-measurements.txt`): both sides' `netcheck`, the DERP→direct flap,
  and `beta`'s double-NAT NAT-PMP response.
