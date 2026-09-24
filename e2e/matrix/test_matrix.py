"""Scenarios of the lumepeer e2e matrix, identical for every [guest, host] pair.

1. connect: the guest dials the host's invite, the host grants full
   control, and the host's picture reaches the guest's view window.
2. mouse: the guest moves the pointer in its view by what is 10 host pixels
   to the right; the host's own cursor has to move by exactly that.
3. keys: the guest types a line of text and a set of chords into its view;
   the host's tracker has to receive the same characters and every chord.
4. hotkeys (Windows guest): the same chords once more, pressed on the
   guest's own keyboard with its grab live, and Ctrl+A/C/V have to copy and
   paste on the host.

What the guest does in 1-3 is dispatched into its view window as DOM events,
key by key and in a person's rhythm, carrying the `key`, `code` and modifiers
a US keyboard produces. From there on everything is real: the view's own
handlers, the IPC, the network, the host's injection into its OS, and the
host's webview receiving the result. OS-level key injection through enigo was
tried and dropped: it types by the guest's *current* layout (Russian here
turned letters into VK_PACKET with no keyup).

That leaves the guest's keyboard grab out of 3, and on a Windows guest the
grab is what carries every chord a person presses (ADR 0107). So 4 clicks
into the view and presses scan codes through SendInput, marked so that the
grab of a pilot build takes them as a person's (agent.py `press_keys`).
"""

import json
import time

import pytest

from harness import (GRAB_JS, KEYS_JS, POINTER_JS, SPY_READ_JS, VIEW_STATE_JS, expected, host_flags, label,
                     os_events, parse, synthetic_events, text_combos, with_logs)

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
    if h.os_click().get("at"):
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


def tracker_or_fail(s, mark):
    """Gives the host's tracker the keyboard, or fails the test saying why not."""
    h = s.host
    if not focus_tracker(s):
        fg = h.foreground()
        on_top = f"; {h.name}'s keyboard is with '{fg['title']}' ({fg['exe']})" if fg and not fg["app"] else ""
        fail(with_log(f"host tracker never got the keyboard focus: neither raising it on {h.os} nor a click "
                      f"gave it one{on_top}; keys untestable", s, mark))


def missing_chords(events, taken):
    """The chords of CHORDS that the tracker's `events` do not show with
    their modifiers, each with what it did see."""
    downs = [(k, c, m) for kind, k, c, m in events if kind == "d" and not is_modifier(c)]
    missing = []
    for combo in CHORDS:
        if combo in taken:
            continue
        code, mods = expected(combo)
        seen = mods_of(downs, code, parse(combo)[1].lower())
        if mods not in seen:
            missing.append(f"{label(combo)}(saw {','.join(seen) or 'nothing'})")
    return missing


def test_keys(session, request):
    s = live(session)
    g, h = s.guest, s.host
    mark = h.log_mark()
    tracker_or_fail(s, mark)
    # A chord another program holds as a global hotkey reaches no window
    # at all, so its absence says nothing about lumepeer.
    taken = h.taken_hotkeys(CHORDS)
    g.js(GRAB_JS % "false", window=s.view)
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

    missing = missing_chords(events, taken)
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


# ── 4 ───────────────────────────────────────────────────────────────────────

PASTE = "lumepeer"
# Select all, copy, to the end, paste: the tracker's text doubles only if the
# host acted on the chords as the commands they are, not merely saw keys.
COPY_PASTE = ["Control+a", "Control+c", "Control+End", "Control+v"]


def test_hotkeys(session, request):
    s = live(session)
    g, h = s.guest, s.host
    if g.os != "windows":
        pytest.skip(f"no keyboard grab on a {g.os} guest")
    if s.picture_error:
        fail(f"no picture to click into ({s.picture_error})")
    mark, gmark = h.log_mark(), g.log_mark()
    tracker_or_fail(s, mark)
    taken = h.taken_hotkeys(CHORDS)
    # On, as it is by default: the keys test released it for the whole app.
    g.js(GRAB_JS % "true", window=s.view)
    # A person's click into the picture, on the host's tracker, is what gives
    # the view the keyboard and puts the grab in charge of it.
    x, y = s.view_point(*h.tracker_point())
    dpr = g.js(VIEW_STATE_JS, window=s.view)["dpr"]
    click = g.os_click(view=True, x=x * dpr, y=y * dpr)
    if not click.get("focused"):
        fail(f"a click into {g.name}'s view window did not give it the keyboard: {click}")
    time.sleep(0.5)
    if not g.grab_live():
        fail(with_logs(f"{g.name}'s keyboard grab is not live after a click into its view", [g], {g.name: gmark}))

    h.tracker_reset(block=True)
    g.js(SPY_READ_JS, window=s.view)
    pressed = g.press_keys(CHORDS)
    time.sleep(1.5)
    events = (h.tracker() or {}).get("keys") or []
    spy = g.js(SPY_READ_JS, window=s.view) or {}
    missing = missing_chords(events, taken)
    grab_after = "live" if g.grab_live() else "NOT live"

    h.tracker_reset(block=False, text=PASTE)
    pasted = g.press_keys(COPY_PASTE)
    deadline = time.monotonic() + 5
    got = ""
    while time.monotonic() < deadline:
        got = (h.tracker() or {}).get("text") or ""
        if got == PASTE * 2:
            break
        time.sleep(0.3)
    h.tracker_reset(block=False)

    problems = []
    if missing:
        problems.append(f"chords missing {len(missing)}/{len(CHORDS) - len(taken)}: " + " ".join(missing))
    if got != PASTE * 2:
        problems.append(f'Ctrl+A,C,End,V on "{PASTE}" left "{got}", want "{PASTE * 2}"')
    untested = (f" | untested, global hotkeys of another program on {h.name}: " + " ".join(label(c) for c in taken)
                if taken else "")
    if problems:
        on_top = (pressed.get("foreground") or {}).get("title")
        guest = (f"guest pressed {pressed.get('pressed')}/{sum(len(os_events(c)) for c in CHORDS)} key events "
                 f"(copy/paste {pasted.get('pressed')}/{sum(len(os_events(c)) for c in COPY_PASTE)}), its grab "
                 f"is {grab_after}, '{on_top}' has its keyboard; the view forwarded "
                 f"{guest_calls(spy, 'input_press')} itself (the grab sends chords; Ctrl/Alt/Shift are "
                 f"the view's to see and drop)")
        fail(with_logs(" | ".join(problems + [guest]) + host_flags(h, s.peer) + untested, [g, h],
                       {g.name: gmark, h.name: mark}))
    note(request, f"{len(CHORDS) - len(taken)} chords ok through the grab, copy/paste ok{untested}")
