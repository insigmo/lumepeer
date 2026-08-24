# ADR 0026 — Direct paths by default, and a release build that can be diagnosed

Status: accepted
Date: 2026-08-24
Supersedes: ADR 0020 (relay-only transport while the WAN is verified)

## Context

v0.0.14 was installed from the GitHub release on two Windows machines on
different networks and different public IPs. The guest could not connect. The
only thing it said was:

> the host refused the connection

which was false in every sense: the host had refused nothing. Four things had
to be wrong at once for that sentence to be the whole of what the product could
tell its user.

**1. The shipped client was relay-only.** ADR 0020 cleared the direct IP
transports by default so that a session between two machines on one LAN could
not silently prove the wrong thing. It was written as temporary and reverted by
one environment variable — but an installed client has no environment. Every
build after it shipped with the WAN test switched on permanently, and with
`clear_ip_transports()` there is no second route: if the relay link is
unhealthy, nothing works at all.

**2. One of the two machines could not hold a relay link.** Reproduced outside
the app with `crates/net/examples/wan_probe.rs` — the real stack, no UI, no
capture:

| Host | Guest | Transport | Result |
| --- | --- | --- | --- |
| machine A | machine A | relay-only | ok, path relay rtt 118 ms |
| machine B | machine A | relay-only | `dial failed: timed out` |
| machine A | machine B | relay-only | host ok (ticket verified, grant sent); guest `stream i/o failed: connection lost` |
| machine A | machine B | relay-only, `LUMEPEER_RELAY_URL=use1-1` | same |

With `iroh=debug` on machine B the cause is explicit and repeatable:
`Lost connection to relay server: Ping timeout`, roughly 16 s after the session
starts carrying data and every ~20 s after that, on `euc1-1` and `use1-1`
alike. Idle, the same TCP:443 connection stays `ESTABLISHED` indefinitely; the
machine's own `tailscale netcheck` reports UDP working, a non-symmetric NAT and
DERP latencies of 91–96 ms. Whatever kills the link — router, ISP, a middlebox
that dislikes a long-lived busy TLS stream — is outside this repository. That
it took the whole product down with it is not.

**3. Nothing in the release build could be read.** `main.rs` sent tracing to
stdout, and a release binary is built `windows_subsystem = "windows"`, whose
stdout is attached to nothing. §16.1 promises structured JSON to a rotating
file; no code implemented it. Diagnosing the failure above required relaunching
the *installed* client from a shell with a redirected handle.

**4. The error the user saw named the wrong machine.** `IpcError::net`
collapses everything that is not a bad ticket, an unreachable host or a version
mismatch into `REJECTED`, deliberately, so that a stranger probing a ticket
learns nothing. But `NetError::Io` is not the host's verdict — it is this
side's own observation that a stream stopped — and reporting it as a refusal
sends the user to inspect the wrong end of the connection.

Once the transport was fixed by hand (`LUMEPEER_LAN_DIRECT=1` on both sides,
the pre-revert spelling), the session connected — and stayed blank, because the
release matrix built Windows with `capture-windows,encode-mf` only. On a
machine with no hardware H.264 encoder `select_encoder` then fails with *"no
hardware encoder and the openh264 fallback is not built in"*, the host logs it
once per media dial, and the guest retries for its full recovery window. The
release CI comment warned about exactly this class of mistake for capture and
then made it for the encoder.

## Decision

Five changes, one per failure above plus the config layer they all needed.

1. **Direct paths are the default again.** `PeerEndpoint::bind` calls
   `bind_with_lan`; `bind_relay_only` survives as an opt-in through
   `LUMEPEER_RELAY_ONLY` (or `prefer_direct = false`), which is how the WAN
   question of ADR 0020 gets asked the next time it matters. `LUMEPEER_LAN_DIRECT`
   is gone: the switch now names the exception, not the norm.

2. **A release build logs to a rotating file** (`logging.rs`): structured JSON
   under the per-user data directory, rotating by `LOG_ROTATION_DAYS` and
   `LOG_ROTATION_MAX_MIB` (§14, §16.1), pruning only files it wrote itself. A
   log directory that cannot be created falls back to stdout and never fails
   the start. Development keeps human-readable stdout.

3. **`NetError::Io` gets its own IPC code**, `TRANSPORT_LOST`. It describes
   what this client observed, so it discloses nothing about the far side's
   decision and the §18 collapse into `REJECTED` stays intact for everything
   that is a decision.

4. **Windows release builds carry both encoders**
   (`capture-windows,encode-mf,encode-openh264`), in `release.yml` and
   `Taskfile.yml` alike. Media Foundation stays preferred; openh264 is what a
   machine without a hardware encoder falls back to instead of staying blank.

5. **The configuration file is actually read** (`config.rs`).
   `config/default.toml` claimed `prefer_direct`, a relay URL and a log
   directory that no code loaded — docs/relay-deployment.md even documented
   `[network].relay_url` as the way to point clients at a self-hosted relay.
   Files are read in order (repository `config/`, next to the executable, then
   the per-user `config.toml`, then `LUMEPEER_CONFIG`), later ones winning key
   by key; a file that does not parse is skipped whole rather than applied in
   half. `LUMEPEER_RELAY_URL` still wins over the file.

## Consequences

- Sessions between machines that can hole-punch no longer touch a relay, so
  they are faster and cost no relay bandwidth — and a broken relay link
  degrades a session instead of preventing it.
- The hazard ADR 0020 named comes back with the direct paths: before a relay
  is reached the address set already holds local interfaces, so an invite
  issued in that window is dialable across the room and nowhere else. The
  `NetError::Offline` guard still refuses an invite with no address at all,
  and `on_invite_create` now warns in the log when the ticket carries no relay
  address, rather than letting the failure surface on the guest's machine.
- ADR 0020's guarantee is lost by design: a passing test between two machines
  on one network no longer proves the internet path works. Asking that question
  now requires `LUMEPEER_RELAY_ONLY=1`, and `wan_probe` prints which mode it is
  in on its first line.
- The client writes to disk in a place it did not write before. Log files carry
  what §15 already allows in logs — pseudonymized peer tags, never addresses or
  device names — and expire after `LOG_ROTATION_DAYS`.
- The config search path means a per-user `config.toml` can now point a client
  at a relay. It cannot widen any grant: nothing in `Settings` reaches consent,
  roles or policy, which stay in `lumepeer-core` (§2.3).
- Windows builds link openh264, which makes the bundle larger and the build
  slower on both Windows runners.

## Verification

- `wan_probe` between the two machines, relay-only, reproduced the field
  failure and the exact `NetError::Io` that surfaced as "the host refused the
  connection"; the same probe run host-and-guest on one machine passed, which
  is what separated the transport from the code.
- With direct paths restored, the installed clients completed invite → consent
  → view window between the same two machines, and the host log then named the
  encoder as the reason the picture never arrived — the failure this ADR's
  fourth change removes.
- `cargo test -p lumepeer-desktop` covers the config layering, the relative log
  directory never landing in the working directory, the rotation file naming
  and that pruning removes only this app's files.
