"""Runner side of the lumepeer e2e matrix: machines, the app on them, sessions.

Everything here talks to a machine through its agent (agent.py): a python
process on that machine, spoken to over stdin/stdout, locally or through
`ssh -T`. The agent talks to the app's tauri-pilot socket; this module only
ever sends it JSON-RPC calls and reads the answers.
"""

import json
import queue
import re
import subprocess
import sys
import threading
import time
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
OUT = REPO / "target" / "e2e" / "matrix"
SSH_OPTS = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15"]
REMOTE_AGENT = ".lumepeer-e2e-agent.py"


class HostError(Exception):
    """A machine, its agent or its app did not answer as it should."""


class Refused(Exception):
    """An IPC command answered with a structured refusal (`code`)."""

    def __init__(self, command, code, detail=""):
        super().__init__(f"{command}: {code}{' ' + detail if detail else ''}")
        self.code = code


REPORTER = None  # pytest's terminal reporter, set by conftest.py


def log(msg):
    """Progress for whoever is watching the run, past pytest's capture."""
    if REPORTER:
        REPORTER.write_line(f"  - {msg}")
    else:
        print(f"  - {msg}", file=sys.__stderr__, flush=True)


def load_config(path=None):
    with open(path or HERE / "hosts.toml", "rb") as f:
        return tomllib.load(f)


# ── the agent ───────────────────────────────────────────────────────────────


class Agent:
    def __init__(self, name, ssh, python, shell=None):
        self.name = name
        if shell:
            cmd = list(shell)
        elif ssh:
            push = subprocess.run(["scp", "-q", *SSH_OPTS, str(HERE / "agent.py"), f"{ssh}:{REMOTE_AGENT}"],
                                  capture_output=True, text=True, timeout=60)
            if push.returncode != 0:
                raise HostError(f"ssh {ssh}: {(push.stderr or push.stdout).strip().splitlines()[-1:]}")
            cmd = ["ssh", "-T", *SSH_OPTS, ssh, f"{python} -u {REMOTE_AGENT}"]
        else:
            cmd = [sys.executable, "-u", str(HERE / "agent.py")]
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE, text=True, bufsize=1, encoding="utf-8")
        self.answers = queue.Queue()
        self.stderr = []
        threading.Thread(target=self._read, args=(self.proc.stdout, self.answers.put), daemon=True).start()
        threading.Thread(target=self._read, args=(self.proc.stderr, self.stderr.append), daemon=True).start()
        self.next_id = 0

    @staticmethod
    def _read(stream, sink):
        for line in stream:
            sink(line.rstrip("\n"))
        sink(None)

    def call(self, op, timeout=30, **req):
        self.next_id += 1
        req.update(op=op, id=self.next_id)
        try:
            self.proc.stdin.write(json.dumps(req) + "\n")
            self.proc.stdin.flush()
        except OSError as error:
            raise HostError(f"{self.name}: agent is gone ({error}); {' '.join(filter(None, self.stderr[-3:]))}")
        deadline = time.monotonic() + timeout
        while True:
            try:
                line = self.answers.get(timeout=max(0.1, deadline - time.monotonic()))
            except queue.Empty:
                raise HostError(f"{self.name}: agent did not answer '{op}' within {timeout}s") from None
            if line is None:
                raise HostError(f"{self.name}: agent exited; {' '.join(filter(None, self.stderr[-3:]))}")
            answer = json.loads(line)
            if answer.get("id") != self.next_id:
                continue  # a late answer to a call that already timed out
            if "err" in answer:
                raise HostError(f"{self.name}: {answer['err']}")
            return answer["ok"]

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            self.proc.kill()


# ── JavaScript run inside the app's windows ─────────────────────────────────

# An IPC call that always resolves, so a refusal comes back as data with its
# code instead of as "[object Object]" in a pilot error.
IPC_JS = """(async () => {
  try {
    const value = await window.__TAURI_INTERNALS__.invoke(%s, %s);
    return { ok: value === undefined ? null : value };
  } catch (e) {
    return { err: String((e && (e.code || e.message)) || e), detail: (e && e.message) || '' };
  }
})()"""

# The host-side tracker: a textarea over the whole main window that records
# every key event and pointer move reaching this machine's webview. Keys are
# stopped at the window so the app's own shortcuts never fire; `block` also
# cancels their default action, so a chord like Ctrl+Z cannot undo the text
# the previous step typed.
TRACKER_JS = r"""(() => {
  if (window.__e2e) return 'present';
  const st = window.__e2e = { keys: [], ptr: null, block: false };
  const ta = document.createElement('textarea');
  ta.id = 'e2e-tracker';
  ta.placeholder = 'lumepeer e2e tracker: keys and pointer reaching this machine are recorded here';
  ta.setAttribute('style', 'position:fixed;inset:0;width:100vw;height:100vh;z-index:2147483647;margin:0;'
    + 'border:0;padding:12px;box-sizing:border-box;background:#fffbe6;color:#000;font:16px monospace;resize:none');
  document.body.appendChild(ta);
  const mods = (e) => (e.ctrlKey ? 'C' : '') + (e.altKey ? 'A' : '') + (e.shiftKey ? 'S' : '') + (e.metaKey ? 'M' : '');
  const key = (e) => {
    st.keys.push([e.type === 'keydown' ? 'd' : 'u', e.key, e.code, mods(e)]);
    e.stopImmediatePropagation();
    if (st.block) e.preventDefault();
  };
  window.addEventListener('keydown', key, true);
  window.addEventListener('keyup', key, true);
  window.addEventListener('pointermove', (e) => {
    st.ptr = { x: e.screenX * devicePixelRatio, y: e.screenY * devicePixelRatio, n: st.ptr ? st.ptr.n + 1 : 1 };
  }, true);
  return 'installed';
})()"""

TRACKER_READ_JS = """(() => {
  const st = window.__e2e, ta = document.getElementById('e2e-tracker');
  if (!st || !ta) return null;
  return { keys: st.keys, ptr: st.ptr, text: ta.value, focus: document.hasFocus() };
})()"""

TRACKER_RESET_JS = """((block, text) => {
  const st = window.__e2e, ta = document.getElementById('e2e-tracker');
  st.keys = []; st.block = block; ta.value = text; ta.focus();
  return document.hasFocus();
})(%s, %s)"""

# The guest-side spy in a view window: every input_* and terminal_* IPC call
# the window makes, and how it ended. Tauri's IPC goes through `fetch` to
# ipc.localhost (ipc://localhost on macOS/Linux), and `invoke` itself is
# frozen, so fetch is where a call can be watched. `term` tallies what the
# terminal's polls brought, from the body terminal.ts decodes (count:u16, then
# shell:u32 | event:u8 | length:u32 | payload, little endian): output bytes,
# and how many opened / closed / refused records. `termText` is the tail of
# that output as text: what reached the window, drawn or not.
SPY_JS = r"""(() => {
  if (window.__e2eSpy) return 'present';
  const sp = window.__e2eSpy = { calls: {}, errs: [], term: {}, termText: '' };
  const tally = (body) => {
    const v = new DataView(body);
    let at = 2;
    for (let i = 0, n = body.byteLength >= 2 ? v.getUint16(0, true) : 0; i < n && at + 9 <= body.byteLength; i++) {
      const event = v.getUint8(at + 4), len = v.getUint32(at + 5, true);
      const name = ['output', 'opened', 'closed'][event] || 'refused';
      sp.term[name] = (sp.term[name] || 0) + (event === 0 ? len : 1);
      if (event === 0) {
        sp.termText = (sp.termText + new TextDecoder().decode(new Uint8Array(body, at + 9, len))).slice(-4096);
      }
      at += 9 + len;
    }
  };
  const orig = window.fetch;
  window.fetch = function (input) {
    const promise = orig.apply(this, arguments);
    const url = String((input && input.url) || input);
    const m = url.match(/^(?:https?:\/\/ipc\.localhost|ipc:\/\/localhost)\/((?:input|terminal)_[a-z_]+)/);
    if (m) {
      const c = sp.calls[m[1]] = sp.calls[m[1]] || { n: 0, ok: 0, err: 0 };
      c.n++;
      promise.then(async (r) => {
        if (r.headers.get('Tauri-Response') === 'error') {
          c.err++;
          if (sp.errs.length < 4) sp.errs.push(m[1] + ':' + (await r.clone().text()).slice(0, 80));
        } else {
          c.ok++;
          if (m[1] === 'terminal_poll') tally(await r.clone().arrayBuffer());
        }
      }, (e) => { c.err++; if (sp.errs.length < 4) sp.errs.push(m[1] + ':' + e); });
    }
    return promise;
  };
  return 'installed';
})()"""

SPY_READ_JS = "(() => { const s = window.__e2eSpy; const r = s ? JSON.parse(JSON.stringify(s)) : null; if (s) { s.calls = {}; s.errs = []; s.term = {}; s.termText = ''; } return r; })()"

# What the guest's terminal shows: the emulator's rows as text, from the
# DOM renderer, which is what a person looks at; and the panel's status line.
# xterm.js draws on animation frames, which a hidden page (a locked screen, a
# display asleep) does not get, so what it shows then says nothing.
TERMINAL_SCREEN_JS = """(() => {
  const rows = document.querySelector('#terminal-screen .xterm-rows');
  const state = document.querySelector('[data-testid=terminal-state]');
  return { text: rows ? rows.innerText.replace(/\\u00a0/g, ' ') : null,
           state: state ? state.textContent.trim() : null, hidden: document.visibilityState === 'hidden' };
})()"""

# Keys typed into the terminal the way xterm.js reads a keyboard: a character
# from its keypress (`charCode`), Enter from its keydown (`keyCode` 13). A
# constructed event leaves those legacy fields 0, so they are set on the
# event itself. All at once rather than at a person's pace: a hidden page (a
# locked Mac) runs its timers so seldom that a paced line outlasts the eval.
TERMINAL_TYPE_JS = """((text) => {
  const ta = document.querySelector('#terminal-screen .xterm-helper-textarea');
  if (!ta) return -1;
  ta.focus();
  const send = (type, key, legacy) => {
    const e = new KeyboardEvent(type, { key, bubbles: true, cancelable: true });
    for (const [name, value] of Object.entries(legacy)) Object.defineProperty(e, name, { get: () => value });
    ta.dispatchEvent(e);
  };
  for (const ch of text) {
    if (ch === '\\r') {
      send('keydown', 'Enter', { keyCode: 13, which: 13 });
      send('keyup', 'Enter', { keyCode: 13, which: 13 });
    } else {
      const code = ch.charCodeAt(0);
      send('keydown', ch, { keyCode: 0, which: 0 });
      send('keypress', ch, { charCode: code, keyCode: code, which: code });
      send('keyup', ch, { keyCode: 0, which: 0 });
    }
  }
  return text.length;
})(%s)"""

TERMINAL_CLOSE_JS = """(() => {
  const button = document.querySelector('[data-testid=terminal-close]');
  if (button) button.click();
  return !!button;
})()"""

VIEW_STATE_JS = """(() => {
  const c = document.getElementById('screen'), o = document.getElementById('overlay');
  const r = c ? c.getBoundingClientRect() : { left: 0, top: 0, width: 0, height: 0 };
  return { w: c ? c.width : 0, h: c ? c.height : 0, left: r.left, top: r.top, width: r.width,
           height: r.height, overlay: ((o && o.innerText) || '').trim().replace(/\\s+/g, ' ').slice(0, 100),
           focus: document.hasFocus(), dpr: devicePixelRatio };
})()"""

# The guest's keyboard grab (ADR 0090/0107) sends Ctrl, Alt and Shift from
# its OS hook and drops the webview's copies while it is live. Synthetic key
# events never pass that hook, so with the grab live every chord would lose
# its modifiers on the way: the keys test releases it, as Ctrl+Alt+Shift+K
# does, and the hotkeys test, which presses real keys, turns it back on. The
# setting lasts as long as the app.
GRAB_JS = """(async () => {
  const peer = new URLSearchParams(location.search).get('peer');
  return await window.__TAURI_INTERNALS__.invoke('view_keyboard_grab', { args: { peer, on: %s } });
})()"""

# What the guest logs when its grab goes live and when it is released.
GRAB_LIVE = "the system chords now reach the remote machine"
GRAB_RELEASED = "the keyboard grab is released"

# Synthetic pointer events on the element under the point, the way a real
# move would arrive: the view's own handlers map them onto the host's screen.
POINTER_JS = """((type, x, y, button) => {
  const target = document.elementFromPoint(x, y) || document.getElementById('view');
  const init = { clientX: x, clientY: y, button, buttons: type === 'pointerdown' ? 1 : 0, bubbles: true,
                 cancelable: true, composed: true, pointerId: 1, pointerType: 'mouse', isPrimary: true };
  target.dispatchEvent(new PointerEvent(type, init));
  return target.id || target.tagName;
})(%s, %s, %s, 0)"""

# On <body>, not on whatever has the focus: the view's chat box would keep
# them. Spaced like a person types, so the test is about what arrives, not
# about how fast.
KEYS_JS = """(async (events) => {
  for (const [type, key, code, m] of events) {
    document.body.dispatchEvent(new KeyboardEvent(type, { key, code, bubbles: true, cancelable: true,
      ctrlKey: m.includes('C'), altKey: m.includes('A'), shiftKey: m.includes('S'), metaKey: m.includes('M') }));
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  return events.length;
})(%s)"""


# ── keys ────────────────────────────────────────────────────────────────────

MODIFIERS = {"control": ("Control", "ControlLeft", "C"), "shift": ("Shift", "ShiftLeft", "S"),
             "alt": ("Alt", "AltLeft", "A"), "meta": ("Meta", "MetaLeft", "M")}
NAMED = {"left": "ArrowLeft", "right": "ArrowRight", "up": "ArrowUp", "down": "ArrowDown", "home": "Home",
         "end": "End", "enter": "Enter", "tab": "Tab", "escape": "Escape", "backspace": "Backspace"}
PUNCT = {",": "Comma", ".": "Period", "/": "Slash", ";": "Semicolon", "'": "Quote", "-": "Minus", "=": "Equal"}
LABEL = {"control": "Ctrl", "shift": "Shift", "alt": "Alt", "meta": "Meta"}


def text_combos(text):
    """tauri-pilot `press` combos that type `text` on a US layout."""
    out = []
    for ch in text:
        out.append("Space" if ch == " " else f"Shift+{ch.lower()}" if ch.isupper() else ch)
    return out


def parse(combo):
    """`Control+Shift+z` -> ([modifier names], key, code)."""
    *mods, main = combo.split("+")
    mods = [m.lower() for m in mods]
    shift = "shift" in mods
    if main.lower() == "space":
        key, code = " ", "Space"
    elif main.lower() in NAMED:
        key = code = NAMED[main.lower()]
    elif main.isalpha():
        key, code = (main.upper() if shift else main.lower()), "Key" + main.upper()
    elif main.isdigit():
        key, code = main, "Digit" + main
    else:
        key, code = main, PUNCT[main]
    return mods, key, code


def expected(combo):
    """What the host's webview must see for `combo`: (code, modifier letters)."""
    mods, _, code = parse(combo)
    return code, "".join(MODIFIERS[m][2] for m in ("control", "alt", "shift", "meta") if m in mods)


def label(combo):
    mods, _, code = parse(combo)
    return "+".join([LABEL[m] for m in mods] + [code.removeprefix("Key").removeprefix("Arrow")])


HOTKEY_MODS = {"alt": 1, "control": 2, "shift": 4, "meta": 8}  # MOD_* of RegisterHotKey
VK = {"ArrowLeft": 0x25, "ArrowUp": 0x26, "ArrowRight": 0x27, "ArrowDown": 0x28, "Home": 0x24, "End": 0x23,
      "Enter": 0x0D, "Tab": 0x09, "Escape": 0x1B, "Backspace": 0x08, "Space": 0x20, "Comma": 0xBC,
      "Period": 0xBE, "Slash": 0xBF, "Semicolon": 0xBA, "Quote": 0xDE, "Minus": 0xBD, "Equal": 0xBB}


def hotkey_of(combo):
    """`combo` as RegisterHotKey's [modifier mask, virtual key]."""
    mods, _, code = parse(combo)
    vk = ord(code[-1]) if code.startswith(("Key", "Digit")) else VK[code]
    return [sum(HOTKEY_MODS[m] for m in mods), vk]


# Set 1 scan codes of the keys the chords use, where a US keyboard has them.
SCAN = ({c: 0x10 + i for i, c in enumerate("qwertyuiop")} | {c: 0x1E + i for i, c in enumerate("asdfghjkl")}
        | {c: 0x2C + i for i, c in enumerate("zxcvbnm")})
SCAN_NAMED = {"ControlLeft": (0x1D, False), "ShiftLeft": (0x2A, False), "AltLeft": (0x38, False),
              "Enter": (0x1C, False), "Tab": (0x0F, False), "Escape": (0x01, False), "Backspace": (0x0E, False),
              "Space": (0x39, False), "ArrowLeft": (0x4B, True), "ArrowRight": (0x4D, True),
              "ArrowUp": (0x48, True), "ArrowDown": (0x50, True), "Home": (0x47, True), "End": (0x4F, True)}


def os_events(combo):
    """`combo` as a keyboard presses it, modifiers first: [scan code, extended, up]."""
    out = []
    for kind, _, code, _ in synthetic_events(combo):
        scan, extended = (SCAN[code[3:].lower()], False) if code.startswith("Key") else SCAN_NAMED[code]
        out.append([scan, extended, kind == "keyup"])
    return out


def synthetic_events(combo):
    """The DOM events a US keyboard produces for `combo`, modifiers first."""
    mods, key, code = parse(combo)
    held, events = "", []
    for m in mods:
        mkey, mcode, letter = MODIFIERS[m]
        held += letter
        events.append(["keydown", mkey, mcode, held])
    events += [["keydown", key, code, held], ["keyup", key, code, held]]
    for m in reversed(mods):
        mkey, mcode, letter = MODIFIERS[m]
        held = held.replace(letter, "")
        events.append(["keyup", mkey, mcode, held])
    return events


# ── one machine ─────────────────────────────────────────────────────────────


@dataclass
class Host:
    name: str
    cfg: dict
    agent: Agent = None
    info: dict = field(default_factory=dict)
    monitor: dict = None
    down: str = None  # why this machine cannot take part

    @property
    def os(self):
        return self.cfg["os"]

    @property
    def wayland(self):
        return self.info.get("session") == "wayland"

    def describe(self):
        if self.down:
            return f"{self.name}=DOWN({self.down})"
        mon = self.monitor or {}
        size = mon.get("size", {})
        extra = f" {self.info.get('session')}" if self.os == "linux" else ""
        locked = ", screen LOCKED" if self.info.get("locked") else ""
        return (f"{self.name}=ok({self.info.get('version', self.os).strip()}{extra}, "
                f"{size.get('width')}x{size.get('height')}@{mon.get('scaleFactor')}{locked})")

    def screen_locked(self):
        """Whether this machine's screen is locked right now, where the agent
        can tell (macOS): nothing draws in its windows then."""
        try:
            return bool(self.agent.call("hello").get("locked"))
        except HostError:
            return False

    # -- lifecycle --

    def start(self, rust_log):
        cfg = self.cfg
        self.agent = Agent(self.name, cfg.get("ssh"), cfg.get("python", "python" if self.os == "windows" else "python3"),
                           cfg.get("shell"))
        self.info = self.agent.call("hello")
        if self.info.get("error"):
            raise HostError(self.info["error"])
        app = cfg["app"] if cfg.get("ssh") or cfg.get("shell") else str(REPO / cfg["app"])
        self.agent.call("start", timeout=60, exe=app, rust_log=rust_log, env=cfg.get("env", {}))
        deadline = time.monotonic() + 45
        while True:
            answer = self.agent.call("pilot", method="ping")
            if "result" in answer:
                break
            if time.monotonic() > deadline or not self.agent.call("alive")["alive"]:
                tail = self.log_lines(grep=["error", "panic", "fatal"], limit=3)
                raise HostError(f"app never answered on its pilot socket ({answer['error'].get('message')})"
                                f"{'; log: ' + ' | '.join(tail) if tail else ''}")
            time.sleep(1)
        # The socket is up before the main window's webview is.
        def loaded():
            try:
                return self.pilot("eval", script="document.readyState") == "complete"
            except HostError:
                return False

        self.wait(loaded, 45, "main window never loaded")
        self.js(TRACKER_JS)
        # The captured monitor is the primary one; Wayland has no notion of
        # primary, and there the window's own monitor is the best guess.
        self.monitor = (self.ipc("plugin:window|primary_monitor", {"label": "main"})
                        or self.ipc("plugin:window|current_monitor", {"label": "main"}))
        if self.wayland and not self.ipc("plugin:window|is_maximized", {"label": "main"}):
            # No global cursor position on Wayland: the pointer is read off the
            # tracker instead, which therefore has to be under it.
            self.ipc("plugin:window|toggle_maximize", {"label": "main"})

    def ensure_healthy(self, rust_log):
        """Restarts an app that died or panicked (a poisoned lock refuses every
        later call), so one crash fails one pair instead of all that follow.
        The crashed run's log is kept next to the report."""
        try:
            self.ipc("connect_status")
            return
        except (Refused, HostError) as error:
            reason = str(error)
        self.crashes = getattr(self, "crashes", 0) + 1
        lines = self.log_lines(limit=1_000_000)
        (OUT / f"{self.name}.crashed-{self.crashes}.log").write_text("\n".join(lines), encoding="utf-8")
        log(f"{self.name}: restarting the app ({reason[:100]})")
        self.agent.close()
        self.start(rust_log)

    def stop(self):
        if self.agent:
            try:
                self.agent.call("stop", timeout=30)
            except HostError:
                pass
            self.agent.close()

    # -- tauri-pilot --

    def pilot(self, method, timeout=30, **params):
        answer = self.agent.call("pilot", timeout=timeout, method=method, params=params)
        if "error" in answer:
            err = answer["error"]
            raise HostError(f"{self.name}: pilot {method}: {err.get('message', err)}")
        return answer["result"]

    def js(self, script, window="main", timeout=30):
        return self.pilot("eval", timeout=timeout, script=script, window=window)

    def ipc(self, command, args=None):
        answer = self.js(IPC_JS % (json.dumps(command), json.dumps(args or {})))
        if "err" in answer:
            raise Refused(command, answer["err"], answer.get("detail", ""))
        return answer["ok"]

    def windows(self):
        return [w["label"] for w in self.pilot("windows.list")["windows"]]

    def press(self, combo, window):
        """A real OS key event (enigo), after tauri-pilot focuses `window`."""
        self.pilot("press", key=combo, window=window)

    def wait(self, check, timeout, what, detail=lambda: ""):
        deadline = time.monotonic() + timeout
        while True:
            value = check()
            if value:
                return value
            if time.monotonic() > deadline:
                more = detail()
                raise HostError(f"{self.name}: {what} within {timeout}s{': ' + more if more else ''}")
            time.sleep(0.5)

    # -- the tracker --

    def tracker(self):
        return self.js(TRACKER_READ_JS)

    def tracker_reset(self, block, text=""):
        self.js(TRACKER_JS)
        return self.js(TRACKER_RESET_JS % (json.dumps(block), json.dumps(text)))

    def cursor(self):
        """This machine's pointer in physical pixels, or None if unknown."""
        if self.wayland:
            ptr = (self.tracker() or {}).get("ptr")
            return (round(ptr["x"]), round(ptr["y"])) if ptr else None
        pos = self.ipc("plugin:window|cursor_position", {"label": "main"})
        return round(pos["x"]), round(pos["y"])

    def focus_tracker(self):
        """Raises the main window and asks whether the tracker has the keyboard.

        tauri-pilot's `press` focuses the window before it injects; Shift on
        its own is the one key that does nothing wherever it lands. On
        Windows that raises the window without giving its webview the
        keyboard, and only a click does (see agent.py)."""
        try:
            self.press("Shift", "main")
        except HostError:
            pass
        time.sleep(0.3)
        return self.tracker_reset(block=False)

    def tracker_point(self):
        """A screen pixel on the tracker: the middle of the main window."""
        if self.wayland:  # no window positions there; it is maximized
            mon = self.monitor
            return (mon["position"]["x"] + mon["size"]["width"] // 2,
                    mon["position"]["y"] + mon["size"]["height"] // 2)
        pos = self.ipc("plugin:window|inner_position", {"label": "main"})
        size = self.ipc("plugin:window|inner_size", {"label": "main"})
        return pos["x"] + size["width"] // 2, pos["y"] + size["height"] // 2

    def os_click(self, view=False, x=None, y=None):
        """A real click into the app's main window (its middle) or its view
        window (at client pixel x, y), raised above whatever covers it first,
        where the agent can make one (Windows): `at` is where it clicked,
        `focused` whether the window then has the keyboard."""
        try:
            return self.agent.call("focus_window", timeout=60, view=view, x=x, y=y)
        except HostError as error:
            return {"focused": False, "why": str(error)}

    def foreground(self):
        """The window that has this machine's keyboard, where the agent can
        tell (Windows): its title, exe, and whether it is the app's. A
        WebView2 page keeps answering `hasFocus()` with true while another
        program's window sits over it and takes every key."""
        try:
            return self.agent.call("foreground", timeout=30) or None
        except HostError:
            return None

    def taken_hotkeys(self, combos):
        """The combos another program on this machine holds as global
        hotkeys (Windows): their keydown reaches that program and no window."""
        try:
            taken = self.agent.call("hotkeys", timeout=30, chords=[hotkey_of(c) for c in combos])["taken"]
        except HostError:
            return []
        return [c for c, t in zip(combos, taken) if t]

    def press_keys(self, combos):
        """`combos` pressed on this machine's own keyboard, the way its
        keyboard grab sees a person press them (Windows); how many key events
        went in, and which window had the keyboard at the end."""
        events = [e for combo in combos for e in os_events(combo)]
        return self.agent.call("press_keys", timeout=60, events=events)

    def grab_live(self):
        """Whether this machine's keyboard grab is live, by its last word on
        it in the log."""
        lines = self.log_lines(grep=[GRAB_LIVE, GRAB_RELEASED], limit=1)
        return bool(lines) and GRAB_LIVE in lines[-1]

    # -- the app's log --

    def log_mark(self):
        try:
            return self.agent.call("log", limit=0)["size"]
        except HostError:
            return 0

    def log_lines(self, since=0, grep=(), limit=50, after=0):
        try:
            return self.agent.call("log", since=since, grep=list(grep), limit=limit, after=after)["lines"]
        except HostError:
            return []


# ── a session between two machines ──────────────────────────────────────────


@dataclass
class Session:
    guest: Host
    host: Host
    peer: str = None  # the guest as the host names it
    view: str = None  # the guest's view window label
    connect_error: str = None
    picture_error: str = None
    connect_secs: float = 0
    path: str = ""

    @property
    def id(self):
        return f"{self.guest.name}->{self.host.name}"

    def view_point(self, x, y):
        """Where host screen pixel x, y is drawn in the guest's view, in CSS
        pixels of the view, or None when it is not on the captured monitor."""
        v = self.guest.js(VIEW_STATE_JS, window=self.view)
        mon = self.host.monitor
        fx = (x - mon["position"]["x"]) / mon["size"]["width"]
        fy = (y - mon["position"]["y"]) / mon["size"]["height"]
        if not (0 <= fx < 1 and 0 <= fy < 1) or v["w"] <= 1:
            return None
        return v["left"] + fx * v["width"], v["top"] + fy * v["height"]

    def stats(self):
        try:
            rows = self.guest.ipc("connection_stats")
        except (HostError, Refused):
            return ""
        row = rows[0] if rows else {}
        rtt = f" rtt={row['rtt_ms']}ms" if row.get("rtt_ms") is not None else ""
        fps = f" fps={row['fps']}" if row.get("fps") is not None else ""
        return f"{row.get('path')}/{row.get('transport')}{rtt} codec={row.get('codec')}{fps}"


def sessions_of(host):
    try:
        return host.ipc("session_status") or []
    except (Refused, HostError):
        return []


def host_flags(host, peer):
    """The host's own account of a session: what it decides input and the
    picture by."""
    row = next((x for x in sessions_of(host) if x.get("peer_label") == peer), None)
    if not row:
        return ""
    return (f" | host session: role={row.get('role')} input={row.get('input')} "
            f"secure_desktop_active={row.get('secure_desktop_active')}")


def view_console(guest, view):
    """The last errors the guest's view window logged: a decoder that
    refuses the stream says so there, not in the app's log."""
    try:
        errors = guest.pilot("console.getLogs", window=view, level="error", last=2) or []
    except HostError:
        return ""
    text = " / ".join(" ".join(str(a) for a in e.get("args", []))[:150] for e in errors)
    return f" | {guest.name} view console: {text}" if text else ""


def phase_of(guest):
    try:
        return guest.ipc("connect_status")
    except (Refused, HostError) as error:
        return {"phase": f"unreachable ({error})"}


def reset(machine):
    """Ends whatever sessions a previous pair left behind on `machine`."""
    for s in sessions_of(machine):
        try:
            machine.ipc("session_revoke", {"args": {"peer": s["peer_label"]}})
        except Refused:
            pass
    if phase_of(machine).get("pending"):
        try:
            machine.ipc("connect_cancel")
        except Refused:
            pass


def connect(guest, host, picture_timeout, remembered=None):
    """A granted full-control session from `guest` to `host`: through an
    invite, or, given `remembered` (the host's label in the guest's history),
    connected for the terminal alone, which has no picture (ADR 0101)."""
    s = Session(guest, host)
    reset(guest)
    reset(host)
    t0 = time.monotonic()
    marks = {m.name: m.log_mark() for m in (guest, host)}
    try:
        if remembered:
            # The connection of the session before can take a moment to let
            # go after its window closed, and a link that stuttered during it
            # has the guest dialing back in by itself (ADR 0105).
            deadline = time.monotonic() + 20
            while True:
                try:
                    guest.ipc("history_connect", {"args": {"peer": remembered, "terminal_only": True}})
                    break
                except Refused as error:
                    if error.code != "ALREADY_CONNECTED" or time.monotonic() > deadline:
                        raise
                    reset(guest)
                    time.sleep(1)
        else:
            code = host.ipc("invite_create", {"args": {"role": "full_control"}})["code"]
            guest.ipc("invite_connect", {"args": {"ticket": code}})

        def pending():
            ph = phase_of(guest)
            if ph.get("phase") in ("denied", "failed"):
                raise HostError(f"guest dial ended '{ph['phase']}' code={ph.get('code')}")
            return next((x["peer_label"] for x in sessions_of(host) if x.get("state") == "pending"), None)

        s.peer = host.wait(pending, 60, "the guest's request never reached the host",
                           lambda: f"guest connect_status={phase_of(guest)}")
        host.ipc("session_grant", {"args": {"peer": s.peer, "role": "full_control"}})
        guest.wait(lambda: phase_of(guest).get("phase") == "connected", 30, "never connected after the grant",
                   lambda: f"connect_status={phase_of(guest)}")
        s.view = guest.wait(lambda: next((w for w in guest.windows() if w.startswith("view-")), None), 15,
                            "opened no view window", lambda: f"windows={guest.windows()}")
        s.connect_secs = time.monotonic() - t0
    except (HostError, Refused) as error:
        s.connect_error = with_logs(str(error), (host, guest), marks)
        return s
    guest.js(SPY_JS, window=s.view)
    if remembered:
        return s
    if host.wayland:
        log(f"{host.name}: approve the screen-share / remote-control portal dialog on its screen "
            f"(waiting up to {picture_timeout}s)")
    try:
        guest.wait(lambda: guest.js(VIEW_STATE_JS, window=s.view)["w"] > 1, picture_timeout, "no picture in the view",
                   lambda: f"overlay='{guest.js(VIEW_STATE_JS, window=s.view)['overlay']}'")
    except HostError as error:
        s.picture_error = with_logs(str(error) + host_flags(host, s.peer) + view_console(guest, s.view),
                                    (host, guest), marks)
    s.path = s.stats()
    return s


def disconnect(s):
    if s.peer:
        try:
            s.host.ipc("session_revoke", {"args": {"peer": s.peer}})
        except (Refused, HostError):
            pass
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            if not sessions_of(s.host) and not any(w.startswith("view-") for w in s.guest.windows()):
                break
        except HostError:
            break
        time.sleep(0.5)
    for machine in (s.guest, s.host):
        try:
            reset(machine)
        except HostError:
            pass


def interesting_log(machine, since, n=2, words=("warn", "error", "input", "inject", "portal", "denied", "refus")):
    """The last few distinct log lines worth showing next to a failure, a
    panic first: it poisons state and every later refusal is only its echo."""
    raw = machine.log_lines(since=since, grep=["panicked at"], after=1, limit=2)
    if len(raw) == 2:
        where = raw[0].split("panicked at", 1)[1].strip().rstrip(":")
        return [f"PANIC {where}: {raw[1].strip()[:160]}"]
    lines = machine.log_lines(since=since, grep=words, limit=400)
    # tauri-pilot's own injection (enigo) complains on machines it cannot
    # drive; that is the harness talking, not the app.
    lines = [l for l in lines if "enigo" not in l and "tauri_plugin_pilot" not in l]
    lines = [l for l in lines if re.search(r"\b(WARN|ERROR)\b", l)] or lines
    seen, out = set(), []
    for line in reversed(lines):
        text = re.sub(r"\x1b\[[0-9;]*m", "", line)
        text = re.sub(r"^\d{4}-\d\d-\d\dT\S+\s+", "", text)  # the timestamp
        key = re.sub(r"[0-9a-f]{8,}|\d+", "#", text)
        if key not in seen:
            seen.add(key)
            out.append(text[:200])
        if len(out) == n:
            break
    return list(reversed(out))


def with_logs(msg, machines, marks):
    """`msg` and what each machine logged about it since `marks`."""
    for machine in machines:
        lines = interesting_log(machine, marks.get(machine.name, 0))
        if lines:
            msg += f" | {machine.name} log: " + " / ".join(lines)
    return msg
