"""Typing into a virtual machine on the host (win -> beta only).

beta runs VMware Workstation with the `debian` VM in it. A person on the
guest clicks into the VM in the picture and types: plain letters, capitals,
digits, and chords. What the VM receives is read back from inside the VM,
where a full-screen xterm in raw mode writes every byte it is typed to a file
(vm_recorder.sh). Raw bytes are the whole point: a letter typed with a Ctrl
the person never held arrives as the control byte it turned into, so "Ctrl
seems stuck" is visible byte by byte instead of as an impression.

The keys go in the way a person's do: pressed on the guest's own keyboard by
scan code with its keyboard grab live (agent.py `press_keys`), so chords take
the grab's path and plain keys the view's (ADR 0107). They are typed once
with the guest on a US layout and once on a Russian one, because the view
makes each plain key's character by the layout the person last switched to.

    python -m pytest e2e/matrix -k "vmware and win_to_beta" --only win,beta

Needs, on beta: VMware Workstation with the VM running in a window, and
nothing holding VMware's input grab; in the VM: a graphical session on :0 and
xterm (the VM's own ssh is `beta@debian`, or LUMEPEER_E2E_VM_SSH).
"""

import os
import subprocess
import time

import pytest

import base64

from harness import GRAB_JS, OUT, SPY_READ_JS, VIEW_STATE_JS, os_events, parse, with_logs

VM_SSH = os.environ.get("LUMEPEER_E2E_VM_SSH", "beta@debian")
RECORDER = os.path.join(os.path.dirname(__file__), "vm_recorder.sh")
REMOTE_RECORDER = "/tmp/lumepeer-vm-recorder.sh"

CTRL = (0x1D, False)
SHIFT = (0x2A, False)
CAPS_LOCK = (0x3A, False)

# What is typed, in order: words, capitals, digits, the chords a person uses
# in a shell, and the case the report was about — letters typed after a
# chord, and letters typed while Ctrl is held down across several of them.
SEQUENCE = (["h", "e", "l", "l", "o", "Space", "Shift+w", "o", "r", "l", "d", "Space", "4", "2", "Enter",
             "Control+a", "a", "b", "c", "Control+e", "d", "e", "f", "Control+u", "x", "y", "z", "Enter",
             "Alt+b", "q", "Shift+q", "Control+l", "m", "n", "Enter"]
            + ["hold-ctrl:k,w", "a", "s", "d", "hold-ctrl:c", "t", "y", "Enter"])


def fail(msg):
    pytest.fail(msg, pytrace=False)


def note(request, text):
    request.node.user_properties.append(("note", text))


def vm(command, timeout=30):
    out = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", VM_SSH, command],
                         capture_output=True, text=True, timeout=timeout)
    return out.stdout.strip()


def recorder(action):
    return vm(f"bash {REMOTE_RECORDER} {action}")


def recorded():
    return bytes(int(h, 16) for h in recorder("read").split())


def events_of(item):
    """`item` as the key events a keyboard sends: [scan code, extended, up]."""
    if item.startswith("hold-ctrl:"):
        keys = item.removeprefix("hold-ctrl:").split(",")
        return ([[*CTRL, False]] + [e for key in keys for e in os_events(key)] + [[*CTRL, True]])
    return os_events(item)


def bytes_of(item):
    """What a US-layout xterm in raw mode writes for `item`."""
    if item.startswith("hold-ctrl:"):
        return b"".join(bytes([ord(key) & 0x1F]) for key in item.removeprefix("hold-ctrl:").split(","))
    mods, key, code = parse(item)
    if code == "Enter":
        out = b"\r"
    elif code == "Space":
        out = b" "
    else:
        out = key.encode()
    if "control" in mods:
        out = bytes([out[0] & 0x1F])
    if "alt" in mods:
        out = b"\x1b" + out
    return out


def show(data):
    """Bytes as a person reads them: control bytes as ^X, ESC as ^[."""
    return "".join(f"^{chr(b + 64)}" if b < 0x20 else chr(b) if b < 0x7F else f"\\x{b:02x}" for b in data)


SNAPSHOT_JS = "document.getElementById('screen').toDataURL('image/png')"


def snapshot(guest, view, name):
    """The picture in the guest's view, saved next to the report: what the VM
    was showing when the keys went in."""
    try:
        url = guest.js(SNAPSHOT_JS, window=view)
        path = OUT / name
        path.write_bytes(base64.b64decode(url.split(",", 1)[1]))
        return str(path)
    except Exception as error:  # noqa: BLE001 - a missing picture must not hide the result
        return f"no snapshot ({error})"


def first_difference(got, want):
    for i, (a, b) in enumerate(zip(got, want)):
        if a != b:
            return i
    return min(len(got), len(want))


@pytest.mark.parametrize("grab", [False, True], ids=["view", "grab"])
@pytest.mark.parametrize("layout", ["00000409", "00000419"], ids=["us", "ru"])
def test_vmware_typing(session, request, layout, grab):
    s = session
    if s.connect_error:
        pytest.skip("no session")
    g, h = s.guest, s.host
    if (g.name, h.name) != ("win", "beta"):
        pytest.skip("VMware runs on beta")
    if s.picture_error:
        fail(f"no picture to click into ({s.picture_error})")

    subprocess.run(["scp", "-q", "-o", "BatchMode=yes", RECORDER, f"{VM_SSH}:{REMOTE_RECORDER}"],
                   check=True, timeout=30)
    if "started" not in recorder("start"):
        pytest.skip(f"no recorder in the VM ({VM_SSH})")
    raised = h.agent.call("raise_exe", timeout=60, exe="vmware.exe")
    rect = raised.get("rect")
    if not rect:
        pytest.skip("no VMware window on beta")
    mark, gmark = h.log_mark(), g.log_mark()

    # Into the middle of the VM's screen, below VMware's own toolbar: the
    # recorder is full screen, so wherever the click lands inside the VM it
    # lands on the recorder, and a VM with its tools installed hands the
    # pointer back as soon as it leaves.
    x, y = (rect[0] + rect[2]) // 2, (rect[1] + rect[3]) // 2 + 60
    at = s.view_point(x, y)
    if at is None:
        fail(f"the VMware window {rect} is not on the captured screen")
    # With the grab off every key, Ctrl and Alt included, goes the view's way;
    # with it on, the chords go through the grab's hook (ADR 0107). Both are
    # a person's path, depending on the toolbar switch.
    g.js(GRAB_JS % ("true" if grab else "false"), window=s.view)
    dpr = g.js(VIEW_STATE_JS, window=s.view)["dpr"]
    for _ in range(2):
        click = g.os_click(view=True, x=at[0] * dpr, y=at[1] * dpr)
        time.sleep(1.0)
    if not click.get("focused"):
        fail(f"a click into {g.name}'s view window did not give it the keyboard: {click}")
    switched = g.agent.call("keyboard_layout", timeout=30, klid=layout)
    time.sleep(0.5)
    g.os_click(view=True, x=at[0] * dpr, y=at[1] * dpr)
    time.sleep(1.0)
    on_top = (h.foreground() or {}).get("exe")

    recorder("start")
    time.sleep(1.0)
    for _ in range(2):
        g.os_click(view=True, x=at[0] * dpr, y=at[1] * dpr)
        time.sleep(1.0)
    grab_state = "live" if g.grab_live() else "NOT live"
    # The VM keeps its own Caps Lock, and a run that went wrong can leave it
    # on: a probe letter says which way it is, and one press puts it right.
    caps = ""
    for _ in range(2):
        start = len(recorded())
        g.agent.call("press_keys", timeout=60, events=os_events("a"))
        time.sleep(1.5)
        probe = recorded()[start:]
        if probe != b"A":
            break
        caps = "the VM's Caps Lock was on and was switched off; "
        g.agent.call("press_keys", timeout=60, events=[[*CAPS_LOCK, False], [*CAPS_LOCK, True]])
        time.sleep(1.0)
    g.js(SPY_READ_JS, window=s.view)
    before = recorded()
    pressed = g.press_keys([])  # a no-op that says whether the view still has the keyboard
    events = [e for item in SEQUENCE for e in events_of(item)]
    answer = g.agent.call("press_keys", timeout=120, events=events)
    time.sleep(3.0)
    got = recorded()[len(before):]
    spy = g.js(SPY_READ_JS, window=s.view) or {}
    calls = {k: v.get("n") for k, v in (spy.get("calls") or {}).items()}
    picture = snapshot(g, s.view, f"vmware-{layout}.png")
    want = b"".join(bytes_of(item) for item in SEQUENCE)
    g.agent.call("keyboard_layout", timeout=30, klid="00000409")
    recorder("stop")
    h.agent.call("raise_exe", timeout=60, exe="vmware.exe", minimize=True)

    if got != want:
        i = first_difference(got, want)
        fail(with_logs(
            f"[{layout}, grab {'on' if grab else 'off'}] the VM received {show(got)!r}, want {show(want)!r}; first difference at byte {i}: "
            f"got {show(got[i:i + 12])!r} want {show(want[i:i + 12])!r} | {caps}probe 'a' gave {show(probe)!r}; "
            f"guest pressed "
            f"{answer.get('pressed')}/{len(events)} key events with its grab {grab_state}, the view sent {calls}, "
            f"layout switch {switched}, beta's foreground {on_top!r} on layout {raised.get('layout')}, {pressed}, "
            f"picture {picture}",
            [g, h], {g.name: gmark, h.name: mark}))
    note(request, f"[{layout}, grab {'on' if grab else 'off'}] {len(want)} bytes arrived as typed: {show(want)!r}")
