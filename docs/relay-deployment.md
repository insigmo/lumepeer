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
cat > .env <<'EOF'
RELAY_HOST=relay.example.com
RELAY_CONTACT=admin@example.com
EOF
docker compose up -d
```

Requirements:

| Requirement | Why |
| --- | --- |
| DNS A/AAAA record for `RELAY_HOST` | ACME certificate issuance and client discovery |
| Port 80 reachable from the internet | Let's Encrypt HTTP-01 challenge |
| Port 443 reachable from the internet | Client relaying traffic over TLS |

Verify:

```sh
curl -f https://relay.example.com/ping   # -> "pong"
docker compose logs iroh-relay           # look for "cert acquired"
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

`config/local.toml` (gitignored) is the right place per machine. No secret
ever belongs there or in the relay config: the relay holds no keys that can
decrypt anything.

## Operating notes

- **Capacity**: a relayed session costs roughly its media bitrate
  (`ABR_MAX_BITRATE_KBPS`, currently 12 Mbit/s worst case) in both
  directions on the relay's uplink. One small VPS handles a handful of
  relayed sessions; most sessions never relay at all once punching works.
- **Monitoring**: `/ping` answers `pong` over HTTPS when healthy;
  `/metrics` serves Prometheus counters (connections, bytes relayed) if you
  enable it with `--metrics`.
- **Version pinning**: the compose file pins an image tag. Upgrades are a
  normal `image:` bump + `docker compose up -d`; relays are stateless, so
  a restart drops only the sessions currently *relayed* through it — those
  peers re-establish via the reconnect window of §10.
- **Multiple relays**: clients take one `relay_url`. For geo redundancy run
  one relay per region and hand out region-appropriate configs; no special
  clustering exists or is needed.
