#!/usr/bin/env bash
# Builds the Linux pilot app for the e2e matrix, inside WSL Debian (root).
#
#     wsl -d Debian -u root -- bash e2e/matrix/build-linux.sh
#
# Same Debian 13 and glibc as the `debian` VM, which has no toolchain of its
# own; deploy.sh copies the result there. Built with plain cargo rather than
# `tauri build` (the repo's node_modules hold the Windows CLI), so the one
# thing `tauri build` adds is added by hand: `tauri/custom-protocol`, which
# embeds apps/desktop/dist instead of pointing the windows at the vite devUrl.
# That bundle must already be built (`npm run build` in apps/desktop).
#
# TAURI_CONFIG is the CSP override pilot_config.py explains; tauri-build and
# generate_context! both read it.
set -euo pipefail
cd "$(dirname "$0")/../.."
. /root/.cargo/env
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/root/lp-target}
TAURI_CONFIG=$(python3 e2e/matrix/pilot_config.py)
export TAURI_CONFIG
TRIPLE=x86_64-unknown-linux-gnu
OUT=${E2E_LINUX_OUT:-target/e2e/linux}

test -f apps/desktop/dist/index.html || { echo "no apps/desktop/dist: run 'npm run build' in apps/desktop" >&2; exit 1; }

# tauri-build refuses to run until every externalBin exists for the triple.
cargo build -p lumepeer-decoder-worker -p lumepeer-service -p lumepeer-terminal-worker
mkdir -p apps/desktop/src-tauri/binaries "$OUT"
for bin in lumepeer-decoder-worker lumepeer-service lumepeer-terminal-worker; do
  cp "$CARGO_TARGET_DIR/debug/$bin" "apps/desktop/src-tauri/binaries/$bin-$TRIPLE"
  cp "$CARGO_TARGET_DIR/debug/$bin" "$OUT/"
done

cargo build -p lumepeer-desktop \
  --features pilot,tauri/custom-protocol,capture-x11,capture-portal,encode-openh264,decode-openh264
cp "$CARGO_TARGET_DIR/debug/lumepeer-desktop" "$OUT/"
# A debug binary is ~800 MB, mostly DWARF; the symbols stay for backtraces.
strip --strip-debug "$OUT"/lumepeer-*
echo "built: $OUT/lumepeer-desktop"
