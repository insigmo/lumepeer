#!/usr/bin/env bash
# The far end of test_vmware.py: a full-screen xterm inside the VM that writes
# every byte it is typed, raw, to a file the test reads back over ssh.
#
#     vm_recorder.sh start   # a fresh recorder, the file emptied
#     vm_recorder.sh read    # what arrived so far, as hex
#     vm_recorder.sh stop
#
# Raw mode (`stty raw -echo`), so Ctrl+C is the byte 0x03 rather than a signal,
# Enter is 0x0d, and a letter typed with a Ctrl nobody meant to hold shows up
# as the control byte it became. `metaSendsEscape`, so Alt+x is ESC x.
set -u
OUT=/tmp/lumepeer-vm-keys.bin
TITLE=lumepeer-vm-recorder

session_env() {
  export XDG_RUNTIME_DIR=/run/user/$(id -u)
  export DISPLAY=:0
  local auth
  auth=$(ls "$XDG_RUNTIME_DIR"/xauth_* 2>/dev/null | head -1)
  [ -n "$auth" ] || auth=$(ls "$XDG_RUNTIME_DIR"/gdm/Xauthority 2>/dev/null | head -1)
  export XAUTHORITY=$auth
}

case "${1:-}" in
  start)
    pkill -f "xterm -title $TITLE" 2>/dev/null
    : > "$OUT"
    session_env
    nohup xterm -title "$TITLE" -fullscreen -fa Monospace -fs 14 \
      -xrm 'XTerm*metaSendsEscape: true' -xrm 'XTerm*eightBitInput: false' \
      -e bash -c "stty raw -echo; exec dd bs=1 of=$OUT status=none" >/dev/null 2>&1 &
    sleep 1.5
    pgrep -f "xterm -title $TITLE" >/dev/null && echo started || echo "xterm did not start"
    ;;
  read)
    od -An -tx1 -v "$OUT" | tr -s ' \n' ' '
    echo
    ;;
  stop)
    pkill -f "xterm -title $TITLE" 2>/dev/null
    echo stopped
    ;;
  *)
    echo "usage: $0 start|read|stop" >&2
    exit 2
    ;;
esac
