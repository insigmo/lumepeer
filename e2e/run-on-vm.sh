#!/usr/bin/env bash
# E2E smoke test of the desktop client on a remote Linux VM over SSH
# (docs/e2e-testing.md; questions.md item 4).
#
# Usage:   e2e/run-on-vm.sh <host> <session-type>
#          e2e/run-on-vm.sh beta@192.168.40.128 x11
#          e2e/run-on-vm.sh beta@192.168.40.130 wayland
#
# The script runs ON the VM (scp it there or pipe it through ssh bash).
# It syncs nothing: `task remote:e2e:<type>` in Taskfile.yml does the sync,
# then invokes this.
set -euo pipefail

SESSION_TYPE="${1:?usage: run-on-vm.sh <x11|wayland>}"
cd ~/lumepeer

# nvm + cargo env, exactly like lumepeer-env.sh does for task-driven builds.
[ -s "$HOME/.nvm/nvm.sh" ] && . "$HOME/.nvm/nvm.sh"
[ -s "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
export PATH="$HOME/.local/bin:$PATH"

echo "== building frontend =="
(cd apps/desktop && npm install --no-audit --no-fund >/dev/null && npm run build)

echo "== building debug app with pilot =="
case "$SESSION_TYPE" in
  x11)     MEDIA_FEATURES="capture-x11,encode-openh264,decode-openh264" ;;
  wayland) MEDIA_FEATURES="encode-openh264,decode-openh264" ;; # portal capture is opt-in at runtime
  *) echo "unknown session type: $SESSION_TYPE" >&2; exit 2 ;;
esac
cargo build -p lumepeer-desktop --features pilot,"$MEDIA_FEATURES"

BIN=target/debug/lumepeer-desktop
test -x "$BIN"

echo "== locating the graphical session ($SESSION_TYPE) =="
if [ "$SESSION_TYPE" = "wayland" ]; then
  WAYLAND_DISPLAY_VALUE="$(ls /run/user/$(id -u)/ 2>/dev/null | grep -o '^wayland-[0-9]*' | head -1 || true)"
  test -n "$WAYLAND_DISPLAY_VALUE" || { echo "no Wayland display found; is the Wayland session up?" >&2; exit 3; }
  SESSION_ENV=( env "WAYLAND_DISPLAY=$WAYLAND_DISPLAY_VALUE"
                       "XDG_SESSION_TYPE=wayland"
                       "XDG_RUNTIME_DIR=/run/user/$(id -u)" )
else
  DISPLAY_VALUE="$(ls /tmp/.X11-unix/ 2>/dev/null | grep -o '^X[0-9]*$' | head -1 | tr -d 'X')"
  DISPLAY_VALUE=":${DISPLAY_VALUE:-0}"
  SESSION_ENV=( env "DISPLAY=${DISPLAY_VALUE}" )
fi
echo "session env: ${SESSION_ENV[*]}"

echo "== headless identity: encrypted-file keystore =="
# A session reached only over SSH never ran PAM's keyring unlock, so the
# Secret Service default collection is locked and every call ends in
# "prompt dismissed". LUMEPEER_KEYSTORE=file (ADR 0023) selects the
# encrypted-file store instead; the key is derived from /etc/machine-id,
# which no remote peer can read.
export LUMEPEER_KEYSTORE=file
export LUMEPEER_KEYSTORE_PATH="$HOME/.local/share/lumepeer/e2e-identity.keystore"
mkdir -p "$(dirname "$LUMEPEER_KEYSTORE_PATH")"

echo "== launching app under the session =="
pkill -f 'lumepeer-desktop' 2>/dev/null || true
sleep 1
"${SESSION_ENV[@]}" "$BIN" > /tmp/lumepeer-e2e-app.log 2>&1 &
APP_PID=$!
trap 'kill $APP_PID 2>/dev/null || true' EXIT

# Wait for the tauri-pilot socket to answer.
echo "== waiting for pilot socket =="
for i in $(seq 1 30); do
  if tauri-pilot ping >/dev/null 2>&1; then break; fi
  sleep 1
  [ "$i" = 30 ] && { echo "tauri-pilot never answered; app log:" >&2; tail -20 /tmp/lumepeer-e2e-app.log >&2; exit 4; }
done
tauri-pilot ping

echo "== running the scenario =="
mkdir -p target/e2e
RESULT=0
tauri-pilot run e2e/smoke.toml --window main --junit target/e2e/smoke-$SESSION_TYPE.xml || RESULT=$?

echo "== app console errors (if any) =="
tauri-pilot logs --window main --level error || true

# Hold the app open for interactive debugging when asked.
if [ "${E2E_HOLD:-0}" = "1" ] && [ "$RESULT" != "0" ]; then
  echo "holding the app for inspection (kill $APP_PID to end)"
  wait $APP_PID || true
fi

exit $RESULT
