#!/usr/bin/env bash
# Interactive probe of the running app on the X11 VM (debugging aid).
# Assumes e2e/run-on-vm.sh left the app alive (E2E_HOLD=1).
set -euo pipefail
export DISPLAY="${DISPLAY:-:0}"
bash -lc 'tauri-pilot windows' && echo --- && bash -lc 'tauri-pilot snapshot -i --depth 3'
