# Lumepeer

P2P remote desktop and collaboration over Iroh/QUIC with a Tauri client.
Host and guest talk directly; the only central pieces are the Iroh relay
infrastructure, a short-link service and a license broker.

The specification is `p2p-iroh-tauri-design-v12.md`. Section references in the
code (`§8.2`, `§14`, …) point into it, and it wins over anything written here.
Deviations from it live in `docs/adr/`, never in silence.

## Ground rules

- Anything not explicitly permitted is forbidden. The host's Rust core is the
  only thing that authorizes; neither the UI nor the guest can widen a grant.
- `view`, `input`, `clipboard_read`, `clipboard_write`, `file_transfer` and
  `recording` are independent. `FullControl` does not imply recording or files.
- No unattended access, no hidden capture, no bypassing OS permission prompts.
- Every numeric constant lives in `crates/core/src/constants.rs` (§14). Magic
  numbers duplicating them are a defect.

## Layout

| Path | What |
|---|---|
| `crates/core` | Session state machine, consent, permissions, license, audit. TCB. |
| `crates/net` | Iroh endpoint, invite tickets, control framing, reconnect, keystore. |
| `crates/media` | Capture, encode, jitter buffer, adaptive bitrate. |
| `crates/decoder-worker` | Decoder in its own sandboxed OS process (§11.3). |
| `apps/desktop` | Tauri app: `src-tauri` Rust backend, `src` TypeScript webview. |
| `services/broker` | Axum + SQLite license broker. |
| `docs/adr` | Architecture decision records. |
| `ci/resource-budget.yml` | Performance and memory gate for release CI (§15). |

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

Phases 0 and 1 of §19 are done.

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

Still skeletons that return an explicit error rather than pretending to work:
Wayland, Windows and macOS capture, hardware encoding, the native keystore
backends of §11.2, and every broker route.
