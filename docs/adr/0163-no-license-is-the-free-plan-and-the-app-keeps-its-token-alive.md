# ADR 0163 — No license is the free plan, and the app keeps its token alive

Status: accepted
Date: 2026-10-19

Amends §12 (licensing) and [ADR 0006](0006-phase-3-broker-decisions.md)
(broker decisions). Step 1.3 of `docs/monetization-plan.md`.

## Context

§12 specified a license token, an offline guard and a broker, and all three
were written: `crates/core/src/license.rs`, `services/broker`. The app used
none of it. `license_status` was a stub answering `trial`, and
`SessionManager::new()` started every install on `Plan::Trial`, whose 30
minutes of cumulative session time nothing counted. `LicenseGuard` answered
"deny" to an install without a token — correct for a product that only exists
paid, wrong for the plan the owner chose on 2026-10-19: a free personal plan
with no time limit, and paid plans on top of it.

## Decisions

### 1. `Plan::Free` is the plan of an install without a token

`Plan::Free` (wire byte 3, appended after `Team` under the tail-only rule):
one concurrent guest, no time limit, no offline grace because there is
nothing to keep alive. `SessionManager::new()` starts on it.
`LicenseGuard::evaluate` without a token answers `Allow { Free }` instead of
`Deny`, before and regardless of a clock rollback.

### 2. A token that stops being honoured falls back to Free, never to nothing

`LicenseDecision::plan_in_force()` is the token's plan on `Allow` and
`Plan::Free` on `Deny` or `DenyNewSession`. An expired token, a lost seat, an
offline grace that ran out, a clock rolled back: each lowers the install to
Free. Nothing in the license layer stops the app from working.

`Trial` stays in the wire format for tokens the broker may still issue, but no
install is put on it any more, and the heartbeat reports zero active seconds.

### 3. The client lives in `lumepeer-runtime`, the broker's address in the build

`lumepeer_runtime::license_client::LicenseClient` does the four broker calls
of §12.2 over blocking HTTPS (`ureq`), behind a `BrokerApi` trait the tests
replace:

- **activate**: `issue` with the account key the buyer was given and this
  device's id; the token must verify against the broker key *and* carry this
  device's hash (ADR 0006), so a token copied from another install is refused.
- **tick**, every `HEARTBEAT_INTERVAL_SECS`: a heartbeat; `refresh` once the
  token has less than 12 hours left (tokens live 24); `ok: false` or a refusal
  drops the token and says so. An unreachable broker changes nothing but the
  `offline` flag — the guard's grace decides the rest.
- **deactivate**: best-effort `revoke`, then forget the token.

The account key is used once and never stored. The device id is 16 random
bytes and, with the token, the only thing written: both are plain files under
the app's local data directory in `license/`. Neither is a secret — the token
is bound to the device id, and a copied pair is a seat conflict the broker
resolves by heartbeat (ADR 0006).

Where the broker is and which key it signs with come from
`LUMEPEER_BROKER_URL` and `LUMEPEER_BROKER_PUBLIC_KEY` (hex), read at run time
first and at build time second. A build with neither runs on Free and says
activation is unavailable — which is every build until the broker is deployed.

### 4. The desktop app snapshots, the actor admits

`apps/desktop/src-tauri/src/license.rs` holds the client behind a lock and a
snapshot of its status, so `license_status` never waits on the broker. The
heartbeat loop runs on the app's tokio runtime; its first round at start-up
tells the actor the plan (`ActorCommand::SetPlan`), and every later round and
every activation or deactivation does again. A lowered plan does not end
sessions already running; it applies to the next guest admitted.

The sign-in-screen host (ADR 0126/0151) keeps the default, Free.

### 5. The license is a panel in the About section

Plan in force; a key field and **Activate** where this build can activate;
**Deactivate on this device** while a license is held; a note when the server
is unreachable or a license lapsed. The server's own reason text is logged,
not shown: the UI says what it means in the user's language.

## Consequences

- An install with no license behaves exactly as before, minus the label
  `trial`; the guest limit of Free equals Trial's and Pro's.
- Paid plans gate nothing yet except Team's five guests. Which features are
  Pro-only (unattended access, the session report of the plan) is a separate
  decision, and changing it would take features from current users.
- Nothing is live until the broker is deployed with a signing key, its public
  half is set in the release build, and a payment provider's webhook is wired
  to the broker (step 1.2 of the plan).
- `tests/integration/tests/broker.rs` found no broker binary on Windows (no
  `.exe` suffix) and returned early from every test as if it passed; it now
  runs there, and drives `LicenseClient` against the real broker.
