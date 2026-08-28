# Lumepeer

P2P remote desktop and collaboration over Iroh/QUIC with a Tauri client.
Host and guest talk directly; the only central pieces are the Iroh relay
infrastructure, a short-link service and a license broker.

The specification is `p2p-iroh-tauri-design-v12.md`. Section references in the
code (`§8.2`, `§14`, …) point into it, and it wins over anything written here.
Deviations from it live in `docs/adr/`, never in silence. The document itself
is **not in this repository** — it is kept alongside it, so a `§` reference
resolves only for someone who has it. The ADR log is the part that is
self-contained.

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
- No hidden capture, no bypassing OS permission prompts. Unattended access is
  supported (ADR 0033): a trusted device can sign in with a device password
  and an optional one-time code instead of waking someone. The host shows a
  banner it cannot dismiss while that is on, and the session it gets is an
  ordinary one — a role, and none of the independent grants.
- Every numeric constant lives in `crates/core/src/constants.rs` (§14). Magic
  numbers duplicating them are a defect.

## Layout

| Path                     | What                                                                 |
|--------------------------|----------------------------------------------------------------------|
| `crates/core`            | Session state machine, consent, permissions, license, audit. TCB.    |
| `crates/net`             | Iroh endpoint, invite tickets, control framing, reconnect, keystore. |
| `crates/media`           | Capture, encode, jitter buffer, adaptive bitrate.                    |
| `crates/decoder-worker`  | Decoder in its own sandboxed OS process (§11.3).                     |
| `crates/service`         | Privileged helper: Ctrl+Alt+Del delivery, nothing else (ADR 0043).  |
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
`libayatana-appindicator3-dev`, `librsvg2-dev` and `libxdo-dev`, plus
`libpipewire-0.3-dev` and `libclang-dev` for the `capture-portal` and
`audio-capture-pipewire` features that a shipped Linux client carries
(`pipewire-sys` links libpipewire and generates its bindings with bindgen).
The X11 capture path needs no headers of its own — `x11rb` is pure Rust.

At runtime a `.deb`/`.rpm` depends on `libpipewire-0.3-0` (`pipewire-libs` on
RPM distributions) and recommends `xdg-desktop-portal` plus a running
`pipewire`; a Wayland desktop with no portal has no capture path at all.

`apps/desktop/src-tauri/icons/icon.png` is a placeholder until there is a real
one.

## Status

Phases 0 through 6 of §19 are done, except for what needs the
reference-hardware runner of §16.2, a paid vendor relationship or an
independent human tester — each named below. A seventh, unnumbered round then
closed the feature gap against the commercial products (ADR 0023 and the
0029–0040 range); "Written but not wired" at the end lists what it did not
reach.

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

The X11 injection test drives whatever display it runs against, so it is
opt-in. The same switch gates the monitor-enumeration test, which needs a real
X server to have any RandR outputs to enumerate:

```sh
LUMEPEER_TEST_XTEST=1 cargo test -p lumepeer-media --features capture-x11
```

The full set a Linux client actually ships with, which is what CI checks:

```sh
cargo clippy -p lumepeer-media --all-targets --features capture-x11,capture-portal,encode-openh264,audio-opus,audio-capture-pipewire -- -D warnings
```

Windows and macOS have since caught up to that scope: DXGI Desktop Duplication
capture and `SendInput` injection on Windows (ADR 0012), ScreenCaptureKit
capture and `CGEvent` injection on macOS (ADR 0013), the Windows keystore, and
a decoder sandbox on all three platforms — seccomp-BPF, `AppContainer` and
`sandbox_init(3)` (ADR 0019). Hardware encoding is Media Foundation on Windows
(`encode-mf`; ADR 0011) and VA-API on Linux (`encode-vaapi`; ADR 0040); macOS
still encodes in software, as no VideoToolbox backend exists yet. ADR 0007 has
the detail on why phase 4 was scoped to Linux first.

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

Phase 7 (unnumbered in §19, opened against the TeamViewer/AnyDesk/RustDesk gap
list) closed the feature gap. Its decisions are ADR 0023 and the 0028–0040
range; what shipped:

- Grants are issued and withdrawn independently while a session runs, from the
  host's own UI (ADR 0029), and the snapshot rule of §8.2 still holds.
- Clipboard text crosses the wire and the OS in both directions, gated on
  `clipboard_read`/`clipboard_write` (ADR 0030).
- File transfer runs end to end over `rd/file/1`, with `FileTransferStart`
  naming each transfer and the file connection dialed lazily (ADR 0032).
- Unattended access admits a trusted device on an Argon2id device password and
  an optional TOTP code, with a banner the host cannot dismiss (ADR 0033), and
  the address book decides who may even attempt it (ADR 0034).
- Session recording writes the `LMRC` container, and recordings are listed,
  played and exported from the UI (ADR 0031, ADR 0035).
- Connection quality is measured from receiver reports rather than guessed
  from the host's own write latency, driving a degradation ladder (ADR 0037).
- The guest view window carries a real toolbar: fullscreen and scaling with
  hotkeys, cursor-shape updates (ADR 0038), a monitor picker, Ctrl+Alt+Del
  delivery and a microphone back-channel (ADR 0028).
- Audio runs both ways — desktop mix out, guest microphone in — on WASAPI and
  PipeWire (ADR 0023 §5, ADR 0028).
- Linux ships both session types: X11 capture/XTEST and the Wayland portal
  with its PipeWire stream, plus PipeWire audio (ADR 0039).
- Chat rides the control channel with a bounded, non-persisted transcript
  (ADR 0023 §6).
- The audit log of §15 is written: an append-only SQLite table with a 30-day
  retention, peers stored only as a salted hash, and a panel that reads,
  filters, exports and erases it (ADR 0041).
- Releases publish a signed update manifest, the client checks the channel it
  is configured for and installs on a press, and it can start with the user's
  session — which grants nothing on its own and can be switched off from the
  same panel that switched it on (ADR 0042).
- Ctrl+Alt+Del no longer needs the whole client running elevated: a helper
  service with exactly one operation delivers it, and the client falls back to
  its own in-process path when that service is absent (ADR 0043).

### Written but not wired

Code that exists and compiles but that nothing in the product reaches yet.
Named here rather than left to be rediscovered:

- **Privacy mode — decided against.** `MessageKind::PrivacyMode`/
  `PrivacyModeAck` have been in the protocol since minor 1; nothing implements
  them and nothing will. The discriminants stay where they are rather than
  being removed, because every message after them would renumber and the
  golden vectors of §17.2 exist to make exactly that impossible without a
  major version. Read them as reserved, not as pending.
- **macOS audio and monitors.** `platform_audio_capturer` and
  `platform_player` both refuse on macOS, so a macOS host streams no sound and
  plays no guest microphone; `host_monitors()` reports a single primary
  display because nothing enumerates them there.
- **Running before anyone signs in.** The helper service of ADR 0043 is a
  privileged process, but it holds one capability — Ctrl+Alt+Del — and does
  not serve a screen. Reaching a machine before somebody signs in needs a
  session-0 process that hands capture and injection to whichever session
  exists, which is a different piece of work and does not exist here.
- **A released update.** The pipeline is wired end to end — signed artifacts,
  a `latest.json` per release, a per-channel endpoint, a client that checks
  and installs (ADR 0042) — but no release has run through it yet, so nothing
  in it has been proven against a real installed client. The manual steps are
  in `docs/release-checklist.md`.
