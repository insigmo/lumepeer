#!/usr/bin/env bash
# Two-node end-to-end stand: one host and one guest, both real builds of the
# desktop app, on one machine and one display (§19; ADR 0036).
#
# Unlike e2e/smoke.toml — which asserts what a session-less app can honestly
# show — this brings a whole session up and back down through the same IPC
# surface the UI uses: invite, dial, consent, grants, a real recording of the
# host's own screen, its export into playable files, and a revoke. Nothing is
# stubbed: two iroh endpoints, a QUIC connection between them, X11 capture,
# H.264 encode, and the recorder writing to disk.
#
# Usage (CI runs exactly this, under Xvfb):
#     xvfb-run -a e2e/ci-stand.sh
#
# Environment:
#   LUMEPEER_BIN         app binary            (default target/debug/lumepeer-desktop)
#   E2E_OUT              artifacts + JUnit     (default target/e2e)
#   E2E_REQUIRE_VIDEO    fail when the recording carried no picture (default 1)
#   E2E_HOLD             keep both apps alive after a failure, to poke at them
#
# The two nodes never share a directory: each gets its own XDG data, config
# and runtime dir, which is also what gives each its own tauri-pilot socket
# (the plugin names it `$XDG_RUNTIME_DIR/tauri-pilot-<identifier>.sock`) and
# its own identity keystore. Without that separation they would be one node
# talking to itself.
set -euo pipefail

# Every path below — the binary, the bundle, the scenario files — is relative
# to the repository root, so the script may be started from anywhere.
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BIN=${LUMEPEER_BIN:-target/debug/lumepeer-desktop}
OUT=${E2E_OUT:-target/e2e}
REQUIRE_VIDEO=${E2E_REQUIRE_VIDEO:-1}
APP_IDENTIFIER=io.insigmo.lumepeer
# Long enough for a dial that has to fall back to a relay, short enough that a
# hung stand fails the job rather than the job timeout.
CONNECT_TIMEOUT_SECS=${E2E_CONNECT_TIMEOUT_SECS:-60}
# The picture has to arrive, be encoded and be written before the recording is
# stopped, or there is nothing to export.
RECORD_SECS=${E2E_RECORD_SECS:-6}

mkdir -p "$OUT"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/lumepeer-stand.XXXXXX")

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

command -v tauri-pilot >/dev/null || fail "tauri-pilot is not on PATH (cargo install tauri-pilot-cli --version 0.7.2)"
command -v python3 >/dev/null || fail "python3 is not on PATH (this script reads the IPC answers with it)"
[ -x "$BIN" ] || fail "no app binary at $BIN — build it with --features pilot first"
[ -n "${DISPLAY:-}" ] || fail "no DISPLAY — run this under xvfb-run, or on a real session"

# ── the webview bundle ───────────────────────────────────────────────────────
#
# A debug build of a Tauri app loads `build.devUrl`, not the bundle compiled
# into it: `tauri-build` sets `cfg(dev)` for a debug profile and
# `generate_context!` embeds the URL instead of the files. And the pilot
# bridge only exists in a debug build (main.rs gates it on
# `debug_assertions`). So the stand serves `apps/desktop/dist` at that URL
# itself — without it both windows come up blank and every step below times
# out against a page that never loaded. `capabilities/main.json` already
# names `http://localhost:*` as a remote origin for the main window, which is
# what makes the served page able to call the IPC surface at all.
DEV_URL=$(python3 -c "import json; print(json.load(open('apps/desktop/src-tauri/tauri.conf.json'))['build']['devUrl'])")
DEV_PORT=${DEV_URL##*:}
DEV_PORT=${DEV_PORT%%/*}
BUNDLE_PID=

bundle_answers() {
  python3 -c "import sys, urllib.request
try:
    urllib.request.urlopen('$DEV_URL/index.html', timeout=1)
except Exception:
    sys.exit(1)" 2>/dev/null
}

serve_bundle() {
  [ -f apps/desktop/dist/index.html ] ||
    fail "no webview bundle at apps/desktop/dist — run 'npm run build' in apps/desktop first"
  local bind
  # `localhost` may resolve to ::1 first, which a v4-only listener never
  # answers; the dual-stack bind is tried first and the v4 default is the
  # fallback for a host with IPv6 off.
  for bind in '::' '0.0.0.0'; do
    (cd apps/desktop/dist && exec python3 -m http.server "$DEV_PORT" --bind "$bind")       >"$OUT/bundle-server.log" 2>&1 &
    BUNDLE_PID=$!
    for _ in $(seq 1 10); do
      if bundle_answers; then
        echo "== serving apps/desktop/dist at $DEV_URL (bind $bind) =="
        return 0
      fi
      sleep 1
    done
    kill "$BUNDLE_PID" 2>/dev/null || true
    BUNDLE_PID=
  done
  cat "$OUT/bundle-server.log" >&2 || true
  fail "nothing answered at $DEV_URL — is port $DEV_PORT already taken?"
}

# ── the two nodes ────────────────────────────────────────────────────────────

declare -A PIDS=()

socket_of() {
  echo "$WORK/$1/run/tauri-pilot-$APP_IDENTIFIER.sock"
}

# Starts one node and waits for its pilot socket to answer.
start_node() {
  local name=$1 root="$WORK/$1"
  mkdir -p "$root/data" "$root/config" "$root/run" "$root/cache" "$root/logs"
  # The plugin refuses a runtime dir anything but the user can reach, and
  # falls back to /tmp — where the two nodes would collide on one socket name.
  chmod 700 "$root/run"

  echo "== starting $name =="
  env \
    XDG_DATA_HOME="$root/data" \
    XDG_CONFIG_HOME="$root/config" \
    XDG_RUNTIME_DIR="$root/run" \
    XDG_CACHE_HOME="$root/cache" \
    LUMEPEER_LOG_DIR="$root/logs" \
    LUMEPEER_KEYSTORE=file \
    LUMEPEER_KEYSTORE_PATH="$root/identity.keystore" \
    WEBKIT_DISABLE_COMPOSITING_MODE=1 \
    WEBKIT_DISABLE_DMABUF_RENDERER=1 \
    "$BIN" >"$OUT/$name-app.log" 2>&1 &
  PIDS[$name]=$!

  local socket
  socket=$(socket_of "$name")
  for _ in $(seq 1 60); do
    if [ -S "$socket" ] && tauri-pilot --socket "$socket" ping >/dev/null 2>&1; then
      echo "   $name is up (pid ${PIDS[$name]})"
      return 0
    fi
    kill -0 "${PIDS[$name]}" 2>/dev/null || {
      tail -40 "$OUT/$name-app.log" >&2
      fail "$name died before its pilot socket answered"
    }
    sleep 1
  done
  tail -40 "$OUT/$name-app.log" >&2
  fail "$name never answered on $socket"
}

stop_nodes() {
  local name
  for name in "${!PIDS[@]}"; do
    kill "${PIDS[$name]}" 2>/dev/null || true
  done
}

# ── talking to a node ────────────────────────────────────────────────────────

# Evaluates `$2` in the main window of node `$1` and prints what it returned.
#
# Every script here answers with a string rather than a value, so a refusal is
# an ordinary `ERR:<code>` line this script can read instead of a CLI failure
# that would have to be parsed out of prose.
js() {
  local name=$1 script=$2 out
  if ! out=$(tauri-pilot --socket "$(socket_of "$name")" --window main eval "$script" 2>&1); then
    printf 'ERR:pilot-call-failed:%s' "$out"
    return 0
  fi
  printf '%s' "$out"
}

# Invokes one IPC command on node `$1` and prints `OK:<json>` or `ERR:<code>`.
#
# Through `__TAURI_INTERNALS__` rather than `window.__TAURI__`: this app does
# not set `withGlobalTauri`, so the convenience global does not exist in its
# webview — and it should not, because it would hand every script in the page
# an invoke handle. The internals bridge is the same one the app's own bundle
# calls through, so the ACL of `capabilities/main.json` applies to these calls
# exactly as it does to the UI's.
ipc() {
  local name=$1 command=$2 args=${3:-'{}'}
  js "$name" "(async () => {
    try {
      const value = await window.__TAURI_INTERNALS__.invoke('$command', $args);
      return 'OK:' + JSON.stringify(value === undefined ? null : value);
    } catch (error) {
      return 'ERR:' + String((error && error.code) || error);
    }
  })()"
}

# Same, but a refusal fails the stand. Prints the JSON payload alone.
ipc_ok() {
  local answer
  answer=$(ipc "$@")
  case "$answer" in
    OK:*) printf '%s' "${answer#OK:}" ;;
    *) fail "$2 on $1: $answer" ;;
  esac
}

# Reads one field out of a JSON payload on stdin, without assuming `jq` is
# installed. Booleans come back as `true`/`false` rather than Python's
# capitalized spelling, so the comparisons below read like the JSON does.
json_field() {
  python3 -c 'import json, sys
value = json.load(sys.stdin)
for key in sys.argv[1:]:
    if value is None:
        break
    value = value[int(key)] if isinstance(value, list) else value.get(key)
print("" if value is None else json.dumps(value) if isinstance(value, bool) else value)' "$@"
}

# The number of sessions node `$1` reports, or -1 when it could not be asked.
session_count() {
  local answer=
  answer=$(ipc "$1" session_status)
  case "$answer" in
    OK:*) printf '%s' "${answer#OK:}" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))' ;;
    *) printf -- '-1' ;;
  esac
}

# The phase of node `$1`'s own outgoing connect attempt.
phase_of() {
  local answer=
  answer=$(ipc "$1" connect_status)
  case "$answer" in
    OK:*) printf '%s' "${answer#OK:}" | json_field phase ;;
    *) printf 'unreachable' ;;
  esac
}

# Polls `$3` (a shell snippet) once a second until it prints exactly `$4`.
wait_until() {
  local what=$1 timeout=$2 snippet=$3 want=$4 seen=
  for _ in $(seq 1 "$timeout"); do
    seen=$(eval "$snippet" 2>/dev/null || true)
    if [ "$seen" = "$want" ]; then
      echo "   $what: $want"
      return 0
    fi
    sleep 1
  done
  fail "$what never became '$want' (last: '$seen')"
}

# ── the JUnit report CI reads ────────────────────────────────────────────────

STEPS=()
LAST_STEP=
step() {
  echo "== $1 =="
  STEPS+=("$1")
  LAST_STEP=$1
}

write_junit() {
  local failures=$1 failed=${2:-} name cases=
  for name in ${STEPS+"${STEPS[@]}"}; do
    if [ "$name" = "$failed" ]; then
      cases+="    <testcase name=\"$name\"><failure message=\"see the app logs next to this file\"/></testcase>"$'\n'
    else
      cases+="    <testcase name=\"$name\"/>"$'\n'
    fi
  done
  {
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo "<testsuites><testsuite name=\"lumepeer-two-node-stand\" tests=\"${#STEPS[@]}\" failures=\"$failures\">"
    printf '%s' "$cases"
    echo '</testsuite></testsuites>'
  } >"$OUT/stand.xml"
}

on_exit() {
  local code=$?
  if [ "$code" != 0 ]; then
    write_junit 1 "$LAST_STEP"
    echo "== app logs =="
    tail -n 60 "$OUT"/*-app.log 2>/dev/null || true
    if [ "${E2E_HOLD:-0}" = 1 ]; then
      echo "holding both apps; sockets: $(socket_of host) $(socket_of guest)"
      wait
    fi
  else
    write_junit 0
  fi
  stop_nodes
  if [ -n "$BUNDLE_PID" ]; then
    kill "$BUNDLE_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
  exit "$code"
}
trap on_exit EXIT

# ── the scenario ─────────────────────────────────────────────────────────────

step "both nodes start"
serve_bundle
start_node host
start_node guest

step "each node is its own endpoint"
echo "   host: $(ipc_ok host network_status)"
echo "   guest: $(ipc_ok guest network_status)"
# `ready` is about reaching a relay, which a stand on one machine does not
# need: the invite carries direct addresses and the dial is local. It is
# reported rather than asserted, so a runner with no route to the relay fleet
# still runs the whole scenario.

step "the session-less smoke scenario passes against the host"
# e2e/smoke.toml is the declarative half of this suite: what one app can
# honestly assert before anybody has connected to it. Running it here rather
# than from its own job reuses a node that is already up, and keeps the two
# scenarios from drifting apart.
tauri-pilot --socket "$(socket_of host)" --window main   run e2e/smoke.toml --junit "$OUT/smoke.xml" ||
  fail "the smoke scenario failed; see $OUT/smoke.xml"

step "the host issues an invite"
INVITE=$(ipc_ok host invite_create "{ args: { role: 'full_control' } }")
CODE=$(printf '%s' "$INVITE" | json_field code)
[ -n "$CODE" ] || fail "invite_create answered without a code: $INVITE"
echo "   the invite is ${#CODE} characters"

step "the guest dials it"
# `invite_connect` returns as soon as the attempt is under way (ADR 0027), so
# the assertion is on what `connect_status` says afterwards, not on this call.
TICKET_JS=$(python3 -c 'import json,sys; print(json.dumps(sys.argv[1]))' "$CODE")
ipc_ok guest invite_connect "{ args: { ticket: $TICKET_JS } }" >/dev/null
wait_until "the guest's dial" "$CONNECT_TIMEOUT_SECS" "phase_of guest" awaiting_consent

step "the host sees a request and nothing is granted yet"
PENDING=$(ipc_ok host session_status)
PEER=$(printf '%s' "$PENDING" | json_field 0 peer_label)
[ -n "$PEER" ] || fail "no session reached the host: $PENDING"
STATE=$(printf '%s' "$PENDING" | json_field 0 state)
[ "$STATE" = pending ] || fail "the session was '$STATE' before the host decided anything"
echo "   $PEER is waiting"

step "the host grants full control"
ipc_ok host session_grant "{ args: { peer: '$PEER', role: 'full_control' } }" >/dev/null
wait_until "the guest's connection" 30 "phase_of guest" connected

step "the four independent grants are still off"
ACTIVE=$(ipc_ok host session_status)
for grant in clipboard_read clipboard_write file_transfer recording; do
  held=$(printf '%s' "$ACTIVE" | json_field 0 "$grant")
  [ "$held" = false ] || fail "$grant came with the role — it must be independent (§8.2)"
done

step "the guest opened a window for the picture"
tauri-pilot --socket "$(socket_of guest)" windows | grep -q 'view-' ||
  fail "the guest never opened a remote-view window"

step "recording is refused until the grant is on"
REFUSED=$(ipc host recording_toggle "{ args: { peer: '$PEER', on: true } }")
case "$REFUSED" in
  ERR:*) echo "   refused: ${REFUSED#ERR:}" ;;
  *) fail "the host recorded without the recording grant: $REFUSED" ;;
esac

step "the host grants recording and records"
ipc_ok host session_set_grant "{ args: { peer: '$PEER', grant: 'recording', allowed: true } }" >/dev/null
RECORD_PATH=$(ipc_ok host recording_toggle "{ args: { peer: '$PEER', on: true } }" |
  python3 -c 'import json,sys; print(json.load(sys.stdin) or "")')
[ -n "$RECORD_PATH" ] || fail "recording_toggle started nothing"
echo "   writing $RECORD_PATH"
# §2.2 is about what the two people in the session can see without looking for
# it; the stand checks the state the two indicators are driven from.
RUNNING=$(ipc_ok host session_status | json_field 0 recording_active)
[ "$RUNNING" = true ] || fail "the host does not report the recording it is writing"
sleep "$RECORD_SECS"
ipc_ok host recording_toggle "{ args: { peer: '$PEER', on: false } }" >/dev/null

step "the recording is listed by name"
RECORDINGS=$(ipc_ok host recordings_list)
NAME=$(printf '%s' "$RECORDINGS" | json_field 0 name)
[ -n "$NAME" ] || fail "recordings_list found nothing after a recording: $RECORDINGS"
BYTES=$(printf '%s' "$RECORDINGS" | json_field 0 bytes)
[ "${BYTES:-0}" -gt 0 ] || fail "$NAME is empty"
echo "   $NAME, $BYTES bytes"

step "a name that is a path is refused"
for bad in '../escape.lmrc' 'sub/dir.lmrc' 'notes.txt'; do
  answer=$(ipc host recording_export "{ args: { name: '$bad' } }")
  case "$answer" in
    ERR:BAD_RECORDING) ;;
    *) fail "recording_export accepted '$bad': $answer" ;;
  esac
done
echo "   the export takes a name, never a path (§2.3)"

step "the export produces files a player can open"
EXPORTED=$(ipc_ok host recording_export "{ args: { name: '$NAME' } }")
echo "   $EXPORTED"
EXPORT_DIR=$(printf '%s' "$EXPORTED" | json_field dir)
VIDEO=$(printf '%s' "$EXPORTED" | json_field video)
AUDIO=$(printf '%s' "$EXPORTED" | json_field audio)
FRAMES=$(printf '%s' "$EXPORTED" | json_field video_frames)
if [ "$REQUIRE_VIDEO" = 1 ]; then
  [ -n "$VIDEO" ] || fail "the recording carried no picture: capture or encode produced nothing"
  [ -s "$EXPORT_DIR/$VIDEO" ] || fail "$EXPORT_DIR/$VIDEO is missing or empty"
  # An Annex-B elementary stream starts with a start code, which is what makes
  # the file playable at all (ADR 0031).
  head -c 4 "$EXPORT_DIR/$VIDEO" | od -An -tx1 | grep -qE '00 00 (00 )?01' ||
    fail "$VIDEO does not begin with an Annex-B start code"
  echo "   $VIDEO holds $FRAMES frames"
fi
if [ -n "$AUDIO" ]; then
  [ -s "$EXPORT_DIR/$AUDIO" ] || fail "$EXPORT_DIR/$AUDIO is missing or empty"
  head -c 4 "$EXPORT_DIR/$AUDIO" | grep -q OggS || fail "$AUDIO is not an Ogg stream"
  echo "   $AUDIO is an Ogg Opus stream"
fi

step "the panel now offers a re-export"
AGAIN=$(ipc_ok host recordings_list | json_field 0 exported)
[ "$AGAIN" = true ] || fail "the exported recording is not reported as exported"

step "screenshots of both windows"
tauri-pilot --socket "$(socket_of host)" --window main screenshot "$OUT/host-main.png" >/dev/null || true
tauri-pilot --socket "$(socket_of guest)" --window main screenshot "$OUT/guest-main.png" >/dev/null || true

step "the host revokes and the session ends"
ipc_ok host session_revoke "{ args: { peer: '$PEER' } }" >/dev/null
wait_until "the host's session list" 30 "session_count host" 0

step "neither app died"
for name in host guest; do
  kill -0 "${PIDS[$name]}" 2>/dev/null || fail "$name exited during the scenario"
done

echo
echo "PASS: ${#STEPS[@]} steps, artifacts in $OUT"
