#!/usr/bin/env bash
# Probe whether the pilot __callback IPC round-trip works from the page.
set -u
export DISPLAY="${DISPLAY:-:0}"
bash -lc 'tauri-pilot eval "window.__TAURI_INTERNALS__.invoke(\"plugin:pilot|__callback\", {id: 999999, result: JSON.stringify({probe: true})}).then(() => \"invoked\").catch(e => \"ERR \" + e)"'
