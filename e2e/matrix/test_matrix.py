"""Scenarios of the lumepeer e2e matrix, identical for every [guest, host] pair.

1. connect: the guest dials the host's invite, the host grants full
   control, and the host's picture reaches the guest's view window.
2. mouse: the guest moves the pointer in its view by what is 10 host pixels
   to the right; the host's own cursor has to move by exactly that.
3. keys: the guest types a line of text and a set of chords into its view;
   the host's tracker has to receive the same characters and every chord.

What the guest does is dispatched into its view window as DOM events, key
by key and in a person's rhythm, carrying the `key`, `code` and modifiers a
US keyboard produces. From there on everything is real: the view's own
handlers, the IPC, the network, the host's injection into its OS, and the
host's webview receiving the result. OS-level key injection on the guest was
tried and dropped: enigo types by the guest's *current* layout (Russian here
turned letters into VK_PACKET with no keyup) and the guest's keyboard grab
treats injected modifiers differently, so its failures were the harness's.
"""

import json
import time

import pytest

from harness import (GRAB_OFF_JS, KEYS_JS, POINTER_JS, SPY_READ_JS, VIEW_STATE_JS, expected, host_flags, label,
                     parse, synthetic_events, text_combos, with_logs)

TEXT = "Hello, lumepeer 42"
# Chords every host OS delivers to a focused webview and none of them acts
# on: no Win/Cmd (the shell and the menu bar take those first), nothing the
# guest keeps for itself (Ctrl+Alt+Shift+*), nothing that switches desktops
# or opens devtools.
CHORDS = ["Control+a", "Control+c", "Control+v", "Control+z", "Control+Shift+z", "Alt+x", "Control+Alt+j",
          "Shift+Left", "Control+Home", "Control+Enter"]
STEP = 10  # host pixels


def fail(msg):
    pytest.fail(msg, pytrace=False)


def note(request, text):
    request.node.user_properties.append(("note", text))


def live(session):
    if session.connect_error:
        pytest.skip("no session")
    return session


def guest_calls(spy, command):
    c = ((spy or {}).get("calls") or {}).get(command)
    if not c:
        return f"{command}: 0 calls"
    return f"{command} ok={c['ok']}/{c['n']}" + (f" errs={spy['errs']}" if c["err"] else "")


def with_log(msg, s, since):
    """`msg`, the host's view of the session, and what the host logged."""
    return with_logs(msg + host_flags(s.host, s.peer), [s.host], {s.host.name: since})


# ── 1 ───────────────────────────────────────────────────────────────────────


def test_connect(session, request):
    s = session
    if s.connect_error:
        fail(s.connect_error)
    if s.picture_error:
        fail(f"connected in {s.connect_secs:.1f}s ({s.path}) but {s.picture_error}")
    note(request, f"{s.connect_secs:.1f}s {s.path}")


# ── 2 ───────────────────────────────────────────────────────────────────────


def settle(host, before, timeout=3.0):
    """The host's cursor once it has moved away from `before` and stopped."""
    deadline = time.monotonic() + timeout
    now = host.cursor()
    while now == before and time.monotonic() < deadline:
        time.sleep(0.1)
        now = host.cursor()
    time.sleep(0.25)
    return host.cursor()


def test_mouse(session, request):
    s = live(session)
    g, h = s.guest, s.host
    if s.picture_error:
        fail(f"no picture to aim at ({s.picture_error})")
    mark = h.log_mark()
    g.js(SPY_READ_JS, window=s.view)
    v = g.js(VIEW_STATE_JS, window=s.view)
    mon = h.monitor
    width, height = mon["size"]["width"], mon["size"]["height"]
    k = v["width"] / width  # view CSS pixels per host pixel
    cx, cy = v["left"] + v["width"] / 2, v["top"] + v["height"] / 2
    aim = (mon["position"]["x"] + width // 2, mon["position"]["y"] + height // 2)

    def move(x):
        g.js(POINTER_JS % (json.dumps("pointermove"), x, cy), window=s.view)

    before = h.cursor()
    move(cx - 4 * STEP * k)  # somewhere else first, so the next move is a move
    before = settle(h, before)
    move(cx)
    p0 = settle(h, before)
    move(cx + STEP * k)
    p1 = settle(h, p0)
    spy = g.js(SPY_READ_JS, window=s.view)

    where = (f"view {v['width']:.0f}x{v['height']:.0f}css of frame {v['w']}x{v['h']} for host {width}x{height}"
             f"@{mon['scaleFactor']}")
    if p0 is None or p0 == before:
        how = "Wayland tracker saw no pointer" if h.wayland else "cursor stayed at " + str(p0)
        fail(with_log(f"host cursor never moved ({how}) | guest {guest_calls(spy, 'input_pointer_move')} | {where}",
                      s, mark))
    dx, dy = p1[0] - p0[0], p1[1] - p0[1]
    landed = f"center landed {p0} want~{aim}"
    if abs(dx - STEP) > 1 or abs(dy) > 1:
        fail(with_log(f"moved dx={dx} dy={dy}, want dx={STEP}+-1 dy=0+-1 | {landed} | {where} | "
                      f"guest {guest_calls(spy, 'input_pointer_move')}", s, mark))
    note(request, f"dx={dx} dy={dy} ({landed})")


# ── 3 ───────────────────────────────────────────────────────────────────────


def is_modifier(code):
    return code.startswith(("Control", "Shift", "Alt", "Meta"))


def mods_of(downs, code, key):
    """Modifier sets `downs` pressed the key of a chord with ('-' for none).
    A machine that types by character rather than by position reports no
    `code`, so the character stands in for it."""
    return [m or "-" for k, c, m in downs if c == code or (not c and k.lower() == key)]


def type_combos(guest, view, combos):
    """Types `combos` into the guest's view; returns how many key events that was."""
    count = 0
    for combo in combos:
        events = synthetic_events(combo)
        guest.js(KEYS_JS % json.dumps(events), window=view)
        count += len(events)
        time.sleep(0.05)
    return count


def app_on_top(h):
    """Whether the app's own window has the host's keyboard, as the host's
    desktop says (Windows); True where the agent cannot tell."""
    return (h.foreground() or {}).get("app", True)


def focus_tracker(s):
    """Gives the host's tracker the host's keyboard: raised by tauri-pilot,
    else clicked by the host's agent (Windows, where a raised WebView2 still
    lacks the keyboard), else clicked through the session like a person
    would. The page's own `hasFocus()` is not trusted alone: under another
    program's window it goes on answering true."""
    g, h = s.guest, s.host
    if h.focus_tracker() and app_on_top(h):
        return True
    if h.os_click(*h.tracker_point()):
        time.sleep(0.4)
        if h.tracker_reset(block=False) and app_on_top(h):
            return True
    point = None if s.picture_error else s.view_point(*h.tracker_point())
    if point is None:
        return False
    for kind in ("pointermove", "pointerdown", "pointerup"):
        g.js(POINTER_JS % (json.dumps(kind), *point), window=s.view)
    time.sleep(0.6)
    return h.tracker_reset(block=False) and app_on_top(h)


def test_keys(session, request):
    s = live(session)
    g, h = s.guest, s.host
    mark = h.log_mark()
    if not focus_tracker(s):
        fg = h.foreground()
        on_top = f"; {h.name}'s keyboard is with '{fg['title']}' ({fg['exe']})" if fg and not fg["app"] else ""
        fail(with_log(f"host tracker never got the keyboard focus: neither raising it on {h.os} nor a click "
                      f"gave it one{on_top}; keys untestable", s, mark))
    # A chord another program holds as a global hotkey reaches no window
    # at all, so its absence says nothing about lumepeer.
    taken = h.taken_hotkeys(CHORDS)
    g.js(GRAB_OFF_JS, window=s.view)
    g.js(SPY_READ_JS, window=s.view)

    sent_text = type_combos(g, s.view, text_combos(TEXT))
    deadline = time.monotonic() + 8
    got = ""
    while time.monotonic() < deadline:
        got = (h.tracker() or {}).get("text") or ""
        if len(got) >= len(TEXT):
            break
        time.sleep(0.3)
    spy_text = g.js(SPY_READ_JS, window=s.view) or {}

    h.tracker_reset(block=True)
    sent_chords = type_combos(g, s.view, CHORDS)
    time.sleep(1.5)
    events = (h.tracker() or {}).get("keys") or []
    spy_chords = g.js(SPY_READ_JS, window=s.view) or {}
    h.tracker_reset(block=False)

    downs = [(k, c, m) for kind, k, c, m in events if kind == "d" and not is_modifier(c)]
    missing = []
    for combo in CHORDS:
        if combo in taken:
            continue
        code, mods = expected(combo)
        seen = mods_of(downs, code, parse(combo)[1].lower())
        if mods not in seen:
            missing.append(f"{label(combo)}(saw {','.join(seen) or 'nothing'})")

    problems = []
    if got != TEXT:
        problems.append(f'text want "{TEXT}" got "{got}"')
    tested = len(CHORDS) - len(taken)
    if missing:
        problems.append(f"chords missing {len(missing)}/{tested}: " + " ".join(missing))
    untested = (f" | untested, global hotkeys of another program on {h.name}: " + " ".join(label(c) for c in taken)
                if taken else "")
    if problems:
        guest = (f"guest sent {guest_calls(spy_text, 'input_press')} for {sent_text} key events (text), "
                 f"{guest_calls(spy_chords, 'input_press')} for {sent_chords} (chords)")
        fail(with_log(" | ".join(problems + [guest]) + untested, s, mark))
    note(request, f"text ok, {tested} chords ok{untested}")
