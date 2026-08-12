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

Phase 0 of §19 is done: the workspace builds, the constants of §14, the wire
types of §9.1 and the signatures of §8.3/§11.1 are in place, CI runs fmt,
clippy, build, test, audit and deny. Media capture, the Iroh endpoint, the
keystore backends and every broker route are still skeletons that return an
explicit error rather than pretending to work.
