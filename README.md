# Lumepeer

P2P remote desktop and collaboration over Iroh/QUIC with a Tauri client.
Host and guest talk directly; the only central pieces are the Iroh relay
infrastructure, a short-link service and a license broker.

The specification is `p2p-iroh-tauri-design-v12.md`. Section references in the
code (`§8.2`, `§14`, …) point into it, and it wins over anything written here.
Deviations from it live in `docs/adr/`, never in silence.

## Installing

Every push to `master` bumps the patch version, tags it and builds/publishes
that release for Windows (amd64/arm64), Linux (amd64/arm64, `.deb`/`.rpm`)
and macOS (arm64, `.dmg`) — see `.github/workflows/release.yml`. Grab an
installer straight from the [latest release](https://github.com/insigmo/lumepeer/releases/latest),
or use the installer scripts, which detect the OS/arch and install with the
platform's native package manager:

```sh
curl -fsSL https://raw.githubusercontent.com/insigmo/lumepeer/refs/heads/master/install.sh | bash
```

```powershell
irm https://raw.githubusercontent.com/insigmo/lumepeer/refs/heads/master/install.ps1 | iex
```

Pass `--version vX.Y.Z` (`-Version vX.Y.Z` on Windows) to pin a release
instead of installing latest.

## Ground rules

- Anything not explicitly permitted is forbidden. The host's Rust core is the
  only thing that authorizes; neither the UI nor the guest can widen a grant.
- `view`, `input`, `clipboard_read`, `clipboard_write`, `file_transfer` and
  `recording` are independent. `FullControl` does not imply recording or files.
- No unattended access, no hidden capture, no bypassing OS permission prompts.
- Every numeric constant lives in `crates/core/src/constants.rs` (§14). Magic
  numbers duplicating them are a defect.

## Layout

| Path                     | What                                                                 |
|--------------------------|----------------------------------------------------------------------|
| `crates/core`            | Session state machine, consent, permissions, license, audit. TCB.    |
| `crates/net`             | Iroh endpoint, invite tickets, control framing, reconnect, keystore. |
| `crates/media`           | Capture, encode, jitter buffer, adaptive bitrate.                    |
| `crates/decoder-worker`  | Decoder in its own sandboxed OS process (§11.3).                     |
| `apps/desktop`           | Tauri app: `src-tauri` Rust backend, `src` TypeScript webview.       |
| `services/broker`        | Axum + SQLite license broker.                                        |
| `docs/adr`               | Architecture decision records.                                       |
| `ci/resource-budget.yml` | Performance and memory gate for release CI (§15).                    |

Three ALPNs, each on its own QUIC connection: `rd/control/1`, `rd/media/1` and
`rd/file/1`. The file connection opens lazily only after `FileAccept(true)`, so
neither media nor a transfer can delay a revoke.

## Building

```sh
# Webview bundle first: tauri-build reads it at compile time.
cd apps/desktop && npm install && npm run build && cd -

cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Linux needs `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`,
`libayatana-appindicator3-dev`, `librsvg2-dev` and `libxdo-dev`.

`apps/desktop/src-tauri/icons/icon.png` is a placeholder until there is a real
one.

## Status

Phases 0 through 4 of §19 are done; phase 5 is done except for what needs the
reference-hardware runner of §16.2 (see below).

Phase 0: the workspace builds, the constants of §14, the wire types of §9.1 and
the signatures of §8.3/§11.1 are in place, CI runs fmt, clippy, build, test,
audit and deny.

Phase 1: the Iroh endpoint binds and dials all three ALPNs, invite tickets are
signed, encoded as QR strings and single-use, the control stream carries the
framing and anti-replay rules of §9.1, the consent model of §8 runs in memory
behind the five IPC commands, and the keystore ships as a trait with an
in-memory and an encrypted-file backend. Two local instances complete
`Hello`/`HelloAck` -> `ConsentRequest` -> `ConsentGrant` -> `ConsentRevoke` in
`tests/integration`, alongside the guest-limit and queue-overflow tests §17.2
requires and property tests over the §8.1 state machine.

Phase 2 (Linux/X11): real screen capture through `x11rb`, a `CaptureController`
that starts capture with the first viewer and stops it with the last, the
`openh264` software encoder, ABR driven by receiver feedback, and a decoder that
runs as a separate process confined by seccomp-BPF, exchanging frames over a
shared memory ring buffer (§11.3). `tests/integration/tests/media_pipeline.rs`
runs capture -> encode -> sandboxed decode end to end.

The media backends are behind features so the default build needs no platform
SDK:

```sh
cargo test -p lumepeer-media --features capture-x11,encode-openh264
```

Phase 3: the broker serves `/v1/license/issue|heartbeat|revoke|refresh` and
`/v1/webhook/payment` against SQLite, tokens are signed and verified in the
binary format of §12.1, and `LicenseGuard` implements the offline table of
§12.4, including the clock-rollback row. `cargo fuzz` targets live in
`tests/fuzz`, with their corpus replayed on stable by
`tests/integration/tests/protocol_golden.rs`, next to the frozen interop vectors
in `tests/interop/golden_vectors.txt`.

The broker refuses to start without its keys:

```sh
LUMEPEER_BROKER_SIGNING_KEY=<64 hex chars> \
LUMEPEER_BROKER_WEBHOOK_SECRET=<shared secret> \
cargo run -p lumepeer-broker
```

Phase 4 (Linux only, see ADR 0007): `SessionManager::authorize_input` checks
every event before it reaches a platform adapter, the `ControlLimited` allowlist
of §8.2 is snapshotted at grant time so a policy edit cannot widen a running
session, X11 input injection goes through XTEST, the Secret Service keystore of
§11.2 works against a real session keyring, the Wayland portal handshake runs in
the normative order of §11, and every row of the error matrix has its own test
in `tests/integration/tests/error_matrix.rs`.

The X11 injection test drives whatever display it runs against, so it is opt-in:

```sh
LUMEPEER_TEST_XTEST=1 cargo test -p lumepeer-media --features capture-x11
```

Still failing with an explicit error rather than pretending to work: PipeWire
frame consumption on Wayland, everything Windows and macOS specific (capture,
input, keystore, decoder sandbox), and hardware encoding. Each needs a machine
that can build and run it; ADR 0007 lists them.

Phase 5: `cargo audit`/`cargo deny` (already wired since phase 0/3) are joined
by a `cargo cyclonedx` SBOM step in the same `supply-chain` CI job, uploaded as
an artifact on every push. `DecoderHandle::pid()` lets the sandboxed decoder
worker's `VmRSS` be sampled from outside the sandbox;
`tests/integration/tests/resource_budget.rs` drives a real capture -> encode ->
decode loop and checks it against the `active_extra_rss_mib` gate of
`ci/resource-budget.yml`. `docs/release-checklist.md` maps every §21 line to
what actually enforces it today.

What phase 5 does not cover: the acceptance criterion of §19 is the release
build passing every §15 threshold on the reference hardware of §16.2, and no
such self-hosted runner is registered against this repository. The
`resource-budget` CI job is wired up with `if: false` for exactly that reason;
ADR 0008 has the detail, and it is one line to flip once the hardware exists.
Signed artifact verification, also part of the §21 checklist, has nothing
behind it yet either: `tauri.conf.json` carries no bundle signing key, which is
phase 6 work.

Phase 6: the consent and status screens exist as real, tested UI. `apps/
desktop/src/i18n.ts` localizes them in English and Arabic, Arabic chosen
because it is RTL and actually exercises the `dir` switch rather than being
a second LTR translation. `apps/desktop/src/accessibility.test.ts` runs an
axe-core audit against both screens in both locales under jsdom (8/8
passing, zero violations, no markup changes needed), excluding only the two
rules jsdom's lack of a layout engine can't support. `apps/desktop/src/
keyboard-nav.test.ts` confirms every control is a real, reachable `<button>`
and that `Deny` carries the default focus. `tauri.conf.json`'s
`plugins.updater` block signs update artifacts with an Ed25519 key, closing
the gap ADR 0008 flagged; `tauri-plugin-updater` is installed and registered
in `apps/desktop/src-tauri` so that signature is verified at install time
too, though `endpoints: []` since no distribution server exists yet.

What phase 6 does not cover: OS-level installer code signing (Windows
Authenticode, Apple Developer ID + notarization) needs paid vendor
relationships and, for Apple, hardware this repo does not have, so it is not
attempted; and the third-party penetration test §19 phase 6 asks for needs
an independent human tester no CI job or agent session can substitute for,
so Task 7's security-review pass stands in for it instead, imperfectly.
ADR 0009 has the detail on both.
