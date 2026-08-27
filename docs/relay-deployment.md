# Self-hosting the Iroh relay (§7; questions.md item 6)

Decision recorded in questions.md: Lumepeer does **not** ship its own relay
implementation. iroh already contains production relay infrastructure
(`iroh-relay`); what was missing is the deployment story. This document plus
`deploy/docker-compose.yml` is that story.

## What the relay is — and is not

- The relay exists for one case: two peers whose hole punching fails
  (symmetric NATs, restrictive corporate firewalls) still need a publicly
  reachable endpoint to exchange their encrypted QUIC packets through.
- It adds **availability, never trust**. Every payload between host and
  guest is end-to-end encrypted with keys derived from the invite ticket
  handshake (§7, §9.1); the relay only ever sees opaque ciphertext and can
  neither read nor alter session content without breaking QUIC.
- It is not a broker. Licenses, short links and payment webhooks stay on
  `services/broker`; the relay has no database and no state beyond live
  connections.

## Quick start

```sh
cd deploy
# Set `hostname` to the DNS name pointing at this machine and `contact` to an
# address Let's Encrypt can reach:
$EDITOR relay.toml
docker compose up -d
```

The 1.0 relay binary takes exactly two arguments — `--dev` and
`--config-path` — so everything else lives in `deploy/relay.toml`, which the
compose file mounts read-only.

Requirements:

| Requirement                                          | Why                                                                                                          |
|------------------------------------------------------|--------------------------------------------------------------------------------------------------------------|
| DNS A/AAAA record for the `hostname` in `relay.toml` | ACME certificate issuance and client discovery                                                               |
| Port 80/tcp reachable from the internet              | Let's Encrypt HTTP-01 challenge and the captive-portal probe                                                 |
| Port 443/tcp reachable from the internet             | Client relaying traffic over TLS                                                                             |
| Port 7842/udp reachable from the internet            | QUIC address discovery — how a peer learns its own public address, which is what makes hole punching succeed |

Verify:

```sh
curl -f https://relay.example.com/ping   # -> "pong" (HTTPS only; port 80 serves the probe)
docker compose logs iroh-relay           # look for the ACME certificate being acquired
```

## Pointing clients at your relay

Clients pick up relay configuration from `config/default.toml`
(`[network]` section). To use your own relay instead of the default public
one, set:

```toml
[network]
# Self-hosted relay (docs/relay-deployment.md). Peers fall back to this
# endpoint when hole punching fails; direct P2P stays the preferred path.
relay_url = "https://relay.example.com"
```

`config/local.toml` (gitignored) is the right place per machine when running
from a checkout. An **installed** client reads, in order, `config/default.toml`
next to its executable, then the per-user file — `%APPDATA%\io.insigmo.lumepeer\config.toml`
on Windows, `~/Library/Application Support/io.insigmo.lumepeer/config.toml` on
macOS, `$XDG_CONFIG_HOME/io.insigmo.lumepeer/config.toml` on Linux — with later
files winning key by key (ADR 0026). `LUMEPEER_RELAY_URL` overrides all of
them, and `LUMEPEER_CONFIG` names one more file to read last.

No secret ever belongs there or in the relay config: the relay holds no keys
that can decrypt anything.

## Operating notes

- **Capacity**: a relayed session costs roughly its media bitrate
  (`ABR_MAX_BITRATE_KBPS`, currently 12 Mbit/s worst case) in both
  directions on the relay's uplink. One small VPS handles a handful of
  relayed sessions; most sessions never relay at all once punching works.
- **Monitoring**: `/ping` answers `pong` over HTTPS when healthy;
  `/metrics` serves Prometheus counters (connections, bytes relayed) if you
  enable it with `--metrics`.
- **Version pinning**: the compose file pins the image tag to the `iroh`
  version of the workspace manifest (`=1.0.2` today). This is not cosmetic —
  the relay protocol changed across the 0.9x → 1.0 line, so a mismatched relay
  does not serve these clients at all. Bump both in one PR, as §5 requires of
  every iroh pin. Relays are otherwise stateless, so a restart drops only the
  sessions currently *relayed* through it — those peers re-establish via the
  reconnect window of §10.
- **Multiple relays**: clients take one `relay_url`. For geo redundancy run
  one relay per region and hand out region-appropriate configs; no special
  clustering exists or is needed.
