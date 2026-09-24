#!/usr/bin/env python3
"""Per-machine helper of the lumepeer e2e matrix (e2e/matrix/README.md).

Runs on every machine of the matrix, either locally or at the far end of an
`ssh -T`, and answers one JSON request per stdin line with one JSON line on
stdout. The app's tauri-pilot socket is local to each machine (a Unix socket,
or a named pipe on Windows), so something local has to talk to it. That is
this file, and it is why a single pytest process on Windows can drive the
app on a Mac and a Linux box.

Standard library only, because this file is copied as-is to machines that
have nothing but python3.
"""

import glob
import json
import os
import shutil
import socket
import subprocess
import sys
import time

IDENTIFIER = "io.insigmo.lumepeer"
WINDOWS = os.name == "nt"
MACOS = sys.platform == "darwin"
# `dwExtraInfo` of every key press_keys injects ("LUME"). A guest's keyboard
# grab leaves injected keys alone except these, and only in a pilot build
# (E2E_MARK in crates/guestkeys/src/windows_hook.rs).
E2E_MARK = 0x4C554D45
MAIN_TITLE = "Lumepeer"
VIEW_TITLE = "Lumepeer — remote screen"

# Variables that tie a process to the logged-in graphical session on Linux.
# An ssh login has none of them, and an app started without them has no
# display to open and no portal to ask.
SESSION_VARS = (
    "DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS",
    "XAUTHORITY", "XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP",
    "DESKTOP_SESSION", "KDE_FULL_SESSION", "KDE_SESSION_VERSION",
)
SESSION_PROCESSES = ("plasmashell", "kwin_wayland", "gnome-shell", "xfce4-session", "labwc", "Xorg")


class State:
    pid = None
    exe = None
    socket = None
    work = None


def work_dir():
    base = os.environ.get("LOCALAPPDATA") if WINDOWS else os.path.expanduser("~")
    path = os.path.join(base, "lumepeer-e2e" if WINDOWS else ".lumepeer-e2e")
    os.makedirs(path, exist_ok=True)
    return path


def run(cmd, timeout=30):
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    return out.stdout.strip()


# ── the graphical session ───────────────────────────────────────────────────


def windows_session_id():
    import ctypes

    sid = ctypes.c_ulong()
    ctypes.windll.kernel32.ProcessIdToSessionId(os.getpid(), ctypes.byref(sid))
    return sid.value


def linux_session_env():
    """The environment of a process in the user's graphical session."""
    uid = os.getuid()
    candidates = []
    for proc in glob.glob("/proc/[0-9]*"):
        try:
            if os.stat(proc).st_uid != uid:
                continue
            with open(os.path.join(proc, "comm")) as f:
                comm = f.read().strip()
            if comm not in SESSION_PROCESSES:
                continue
            with open(os.path.join(proc, "environ"), "rb") as f:
                raw = f.read().split(b"\0")
        except OSError:
            continue
        env = dict(item.decode(errors="replace").split("=", 1) for item in raw if b"=" in item)
        picked = {k: env[k] for k in SESSION_VARS if k in env}
        if "WAYLAND_DISPLAY" in picked or "DISPLAY" in picked:
            candidates.append((SESSION_PROCESSES.index(comm), picked))
    candidates.sort(key=lambda c: c[0])
    if candidates:
        return candidates[0][1]
    # Started from inside a session (a terminal there, or WSLg): this
    # process already has what it needs.
    own = {k: os.environ[k] for k in SESSION_VARS if k in os.environ}
    return own if "WAYLAND_DISPLAY" in own or "DISPLAY" in own else {}


def op_hello(_):
    info = {"platform": sys.platform, "host": socket.gethostname(), "python": sys.version.split()[0]}
    if WINDOWS:
        info["os"] = "windows"
        info["version"] = "Windows " + run(["cmd", "/c", "ver"]).split("Version ")[-1].rstrip("]")
        info["session0"] = windows_session_id() == 0
    elif MACOS:
        info["os"] = "macos"
        info["version"] = "macOS " + run(["sw_vers", "-productVersion"]) + " " + os.uname().machine
    else:
        info["os"] = "linux"
        env = linux_session_env()
        info["session"] = env.get("XDG_SESSION_TYPE", "none")
        info["desktop"] = env.get("XDG_CURRENT_DESKTOP", "")
        info["version"] = run(["sh", "-c", ". /etc/os-release; echo $PRETTY_NAME"]) + " " + os.uname().machine
        if not env:
            info["error"] = "no graphical session found (nobody logged in at the screen?)"
    return info


# ── starting and stopping the app ───────────────────────────────────────────


def processes_of(exe):
    """Pids of processes running `exe` (an absolute path)."""
    if WINDOWS:
        path = exe.replace("'", "''")
        script = (
            "Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -eq '%s' } "
            "| ForEach-Object { $_.ProcessId }" % path
        )
        out = run(["powershell", "-NoProfile", "-NonInteractive", "-Command", script])
    else:
        out = run(["pgrep", "-f", exe.rstrip("/") + ("/Contents/MacOS/" if exe.endswith(".app") else "")])
    return [int(p) for p in out.split() if p.isdigit()]


def kill(pids):
    for pid in pids:
        if WINDOWS:
            run(["taskkill", "/PID", str(pid), "/T", "/F"])
        else:
            try:
                os.kill(pid, 15)
            except OSError:
                pass
    # taskkill returns before the process is gone, and until it is, its log
    # stays open and cannot be deleted.
    deadline = time.time() + 5
    while time.time() < deadline and any(alive(p) for p in pids):
        time.sleep(0.2)
    for pid in pids if not WINDOWS else ():
        try:
            os.kill(pid, 9)
        except OSError:
            pass


def alive(pid):
    if WINDOWS:
        return str(pid) in run(["tasklist", "/FI", "PID eq %d" % pid, "/NH"])
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def spawn_windows(exe, env, stdout):
    # A debug build is a console program: detached, it gets no console window
    # and its log still reaches `stdout`.
    flags = subprocess.DETACHED_PROCESS | subprocess.CREATE_NEW_PROCESS_GROUP
    subprocess.Popen([exe], env=env, stdout=open(stdout, "w"), stderr=subprocess.STDOUT,
                     stdin=subprocess.DEVNULL, creationflags=flags, cwd=os.path.dirname(exe))


def in_session(args, task, wait=0):
    """Runs this file with `args` in the logged-in user's session, elevated,
    through a scheduled task: the way out of sshd's session 0."""
    pythonw = os.path.join(os.path.dirname(sys.executable), "pythonw.exe")
    cmd = [pythonw if os.path.exists(pythonw) else sys.executable, os.path.abspath(__file__)] + args
    run(["schtasks", "/Create", "/F", "/TN", task, "/SC", "ONCE", "/ST", "23:59", "/RL", "HIGHEST", "/IT",
         "/TR", " ".join('"%s"' % part for part in cmd)])
    out = subprocess.run(["schtasks", "/Run", "/TN", task], capture_output=True, text=True)
    if out.returncode != 0:
        raise RuntimeError("schtasks /Run failed: " + (out.stderr or out.stdout).strip())
    time.sleep(wait)


def socket_path(env):
    if WINDOWS:
        return r"\\.\pipe\tauri-pilot-" + IDENTIFIER
    runtime = env.get("XDG_RUNTIME_DIR")
    if runtime and os.path.isdir(runtime) and os.stat(runtime).st_mode & 0o077 == 0:
        return os.path.join(runtime, "tauri-pilot-%s.sock" % IDENTIFIER)
    return "/tmp/tauri-pilot-%s.sock" % IDENTIFIER


def op_start(req):
    exe = os.path.abspath(os.path.expanduser(req["exe"]))
    if not os.path.exists(exe):
        raise RuntimeError("no app at %s: run e2e/matrix/deploy.sh first" % exe)
    kill(processes_of(exe))
    work = State.work = work_dir()
    logs = os.path.join(work, "logs")
    shutil.rmtree(logs, ignore_errors=True)
    os.makedirs(logs, exist_ok=True)
    # A test identity of its own, so the e2e node never publishes the NodeId
    # of the client installed on the same machine; and permission to run
    # beside that client (both exist only in a pilot build).
    env = {
        "LUMEPEER_ALLOW_SECOND_INSTANCE": "1",
        "LUMEPEER_KEYSTORE": "file",
        "LUMEPEER_KEYSTORE_PATH": os.path.join(work, "identity.keystore"),
        "LUMEPEER_LOG_DIR": logs,
        "RUST_LOG": req.get("rust_log", "info"),
        "NO_COLOR": "1",
    }
    extra = req.get("env") or {}
    env.update(extra)
    stdout = os.path.join(logs, "stdout.txt")

    if WINDOWS and windows_session_id() == 0:
        # sshd hands out session 0, which has no desktop: nothing started from
        # here could capture a screen or show a window. A scheduled task runs
        # this file again in the logged-in user's session, elevated like the
        # shipped client (ADR 0057), and that starts the app.
        spec = os.path.join(work, "launch.json")
        with open(spec, "w") as f:
            json.dump({"exe": exe, "env": env, "stdout": stdout}, f)
        in_session(["--launch", spec], "LumepeerE2E")
    elif WINDOWS:
        # Beside the installed client, which runs elevated and cannot be
        # stopped from an unelevated shell: no UAC prompt nobody is there to
        # accept. A host injecting into its own window needs no elevation.
        spawn_windows(exe, dict(os.environ, __COMPAT_LAYER="RunAsInvoker", **env), stdout)
    elif MACOS and exe.endswith(".app"):
        # Through LaunchServices, so the app is its own "responsible process"
        # and the Screen Recording / Accessibility grants given to it apply.
        # Started straight from sshd, TCC would attribute it to sshd.
        cmd = ["open", "-n", exe, "--stdout", stdout, "--stderr", stdout]
        for key, value in env.items():
            cmd += ["--env", "%s=%s" % (key, value)]
        subprocess.run(cmd, check=True, timeout=30)
    else:
        full = dict(os.environ, **env)
        if not MACOS:
            session = linux_session_env()
            if not session:
                raise RuntimeError("no graphical session to start the app in")
            full.update(session)
            # Its own data and config, so it does not share the history or
            # the address book of an installed client. XDG_RUNTIME_DIR stays
            # the session's: the Wayland socket and the portal live there.
            for var, sub in (("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache")):
                full[var] = os.path.join(work, sub)
                os.makedirs(full[var], exist_ok=True)
            full.update(extra)
            full = {k: v for k, v in full.items() if v != ""}
        subprocess.Popen([exe], env=full, stdout=open(stdout, "w"), stderr=subprocess.STDOUT,
                         stdin=subprocess.DEVNULL, start_new_session=True, cwd=os.path.dirname(exe))
        env = full

    State.exe = exe
    State.socket = socket_path(env)
    deadline = time.time() + 20
    while time.time() < deadline:
        pids = processes_of(exe)
        if pids:
            State.pid = pids[0]
            return {"pid": State.pid, "socket": State.socket}
        time.sleep(0.5)
    raise RuntimeError("the app did not start; %s" % tail_text(stdout, 5))


def op_stop(_):
    if State.exe:
        kill(processes_of(State.exe))
    return {"stopped": True}


def op_alive(_):
    return {"alive": bool(State.exe and processes_of(State.exe))}


def click(x, y):
    """A left click at screen pixel x, y; False if the cursor would not go there."""
    import ctypes

    user32 = ctypes.windll.user32
    user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))  # physical pixels
    moved = user32.SetCursorPos(x, y)
    user32.mouse_event(2, 0, 0, 0, 0)
    user32.mouse_event(4, 0, 0, 0, 0)
    return bool(moved)



def foreground(_):
    """Title, pid and executable of the window that has the keyboard."""
    import ctypes
    from ctypes import wintypes

    user32, kernel32 = ctypes.windll.user32, ctypes.windll.kernel32
    hwnd = user32.GetForegroundWindow()
    title = ctypes.create_unicode_buffer(256)
    user32.GetWindowTextW(hwnd, title, 256)
    pid = wintypes.DWORD()
    user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
    exe = ctypes.create_unicode_buffer(1024)
    size = wintypes.DWORD(1024)
    process = kernel32.OpenProcess(0x1000, False, pid.value)  # PROCESS_QUERY_LIMITED_INFORMATION
    if process:
        kernel32.QueryFullProcessImageNameW(process, 0, exe, ctypes.byref(size))
        kernel32.CloseHandle(process)
    return {"title": title.value, "pid": pid.value, "exe": os.path.basename(exe.value)}


def taken_hotkeys(chords):
    """Which of `chords` ([MOD_* mask, virtual key]) another program holds as
    a global hotkey. Windows hands such a chord's keydown to that program
    and nothing else, so no window, the app's included, ever sees it."""
    import ctypes

    user32 = ctypes.windll.user32
    taken = []
    for mods, vk in chords:
        free = user32.RegisterHotKey(None, 1, mods | 0x4000, vk)  # MOD_NOREPEAT
        if free:
            user32.UnregisterHotKey(None, 1)
        taken.append(not free)
    return taken


def app_window(pids, title):
    """The largest visible top-level window of `pids` titled exactly `title`,
    or None. Largest, because the host's session bar is titled "Lumepeer"
    just like the main window."""
    import ctypes
    from ctypes import wintypes

    user32 = ctypes.windll.user32
    found = []

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def each(hwnd, _):
        pid = wintypes.DWORD()
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
        text = ctypes.create_unicode_buffer(256)
        user32.GetWindowTextW(hwnd, text, 256)
        if pid.value in pids and user32.IsWindowVisible(hwnd) and text.value == title:
            rect = wintypes.RECT()
            user32.GetWindowRect(wintypes.HWND(hwnd), ctypes.byref(rect))
            found.append(((rect.right - rect.left) * (rect.bottom - rect.top), hwnd))
        return True

    user32.EnumWindows(each, 0)
    return max(found)[1] if found else None


def focus_window(arg):
    """A person's click into the app's window titled `title`, at client pixel
    x, y (the middle when not given), the window raised first. Only a click
    gives a WebView2 page the keyboard, and on a guest it is what arms the
    keyboard grab."""
    import ctypes
    from ctypes import wintypes

    user32 = ctypes.windll.user32
    user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))  # physical pixels
    user32.GetForegroundWindow.restype = wintypes.HWND
    hwnd = app_window(arg["pids"], arg["title"])
    if not hwnd:
        return {"focused": False, "why": "no window titled %r" % arg["title"]}
    # To the top of the z-order without asking to be activated, which Windows
    # refuses a process that is not in the foreground; another program's
    # window (a VM's, say) would take the click otherwise.
    flags = 0x0001 | 0x0002 | 0x0010  # SWP_NOSIZE | SWP_NOMOVE | SWP_NOACTIVATE
    for after in (-1, -2):  # HWND_TOPMOST, then HWND_NOTOPMOST
        user32.SetWindowPos(wintypes.HWND(hwnd), wintypes.HWND(after), 0, 0, 0, 0, flags)
    if arg.get("x") is None:
        client = wintypes.RECT()
        user32.GetClientRect(wintypes.HWND(hwnd), ctypes.byref(client))
        point = wintypes.POINT(client.right // 2, client.bottom // 2)
    else:
        point = wintypes.POINT(int(arg["x"]), int(arg["y"]))
    user32.ClientToScreen(wintypes.HWND(hwnd), ctypes.byref(point))
    time.sleep(0.2)
    answer = {"at": [point.x, point.y]}
    if not click(point.x, point.y):
        # VMware Workstation holding its input grab: the cursor is frozen for
        # every program, the click lands in the VM, and nothing short of a
        # real Ctrl+Alt at that machine lets go (an injected one does not).
        answer["cursor"] = "frozen, a VM's input grab?"
    time.sleep(0.5)
    answer["focused"] = user32.GetForegroundWindow() == hwnd
    return answer


def press_keys(arg):
    """Presses `events` ([scan code, extended, up]) on this desktop's keyboard,
    marked so the guest's grab takes them as a person's, while the view window
    has the keyboard. By scan code, so this machine's layout makes of each
    position what it would of a real key."""
    import ctypes
    from ctypes import wintypes

    class KEYBDINPUT(ctypes.Structure):
        _fields_ = [("wVk", wintypes.WORD), ("wScan", wintypes.WORD), ("dwFlags", wintypes.DWORD),
                    ("time", wintypes.DWORD), ("dwExtraInfo", ctypes.c_size_t)]

    class MOUSEINPUT(ctypes.Structure):  # only for the size of INPUT's union
        _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG), ("mouseData", wintypes.DWORD),
                    ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD), ("dwExtraInfo", ctypes.c_size_t)]

    class UNION(ctypes.Union):
        _fields_ = [("ki", KEYBDINPUT), ("mi", MOUSEINPUT)]

    class INPUT(ctypes.Structure):
        _fields_ = [("type", wintypes.DWORD), ("u", UNION)]

    user32 = ctypes.windll.user32
    user32.GetForegroundWindow.restype = wintypes.HWND
    hwnd = app_window(arg["pids"], VIEW_TITLE)
    if not hwnd or user32.GetForegroundWindow() != hwnd:
        return {"pressed": 0, "foreground": foreground(None)}
    pressed = 0
    for scan, extended, up in arg["events"]:
        key = INPUT(type=1)  # INPUT_KEYBOARD
        # KEYEVENTF_SCANCODE, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP
        key.u.ki = KEYBDINPUT(0, scan, 0x0008 | (0x0001 if extended else 0) | (0x0002 if up else 0), 0, E2E_MARK)
        pressed += user32.SendInput(1, ctypes.byref(key), ctypes.sizeof(INPUT))
        time.sleep(0.04)  # a person's rhythm
    return {"pressed": pressed, "foreground": foreground(None)}


QUERIES = {"foreground": foreground, "hotkeys": taken_hotkeys, "focus_window": focus_window, "press_keys": press_keys}


def on_desktop(name, arg):
    """Answers query `name` on the logged-in desktop: here, or from sshd's
    session 0 through a scheduled task, where both answers would be about a
    desktop nobody sees."""
    if windows_session_id() != 0:
        return QUERIES[name](arg)
    # By name only: the task's command line has to fit schtasks' 261 characters.
    out = os.path.join(work_dir(), name + ".json")
    try:
        os.remove(out)
    except OSError:
        pass
    with open(out + ".in", "w") as f:
        json.dump(arg, f)
    in_session(["--query", name], "LumepeerE2EQuery")
    deadline = time.time() + 15
    while time.time() < deadline:
        try:
            with open(out) as f:
                return json.load(f)
        except (OSError, ValueError):
            time.sleep(0.2)
    raise RuntimeError("the %s query on the desktop never answered" % name)


def op_foreground(_):
    if not WINDOWS:
        return {}
    answer = on_desktop("foreground", None)
    answer["app"] = bool(State.exe) and answer["pid"] in processes_of(State.exe)
    return answer


def op_hotkeys(req):
    if not WINDOWS:
        return {"taken": [False] * len(req["chords"])}
    return {"taken": on_desktop("hotkeys", req["chords"])}


def op_focus_window(req):
    if not WINDOWS:
        return {"focused": False, "why": "only on Windows"}
    title = VIEW_TITLE if req.get("view") else MAIN_TITLE
    return on_desktop("focus_window", {"pids": processes_of(State.exe), "title": title,
                                       "x": req.get("x"), "y": req.get("y")})


def op_press_keys(req):
    if not WINDOWS:
        return {"pressed": 0}
    return on_desktop("press_keys", {"pids": processes_of(State.exe), "events": req["events"]})


# ── tauri-pilot ─────────────────────────────────────────────────────────────


REQUEST_ID = [0]


def op_pilot(req):
    """One JSON-RPC call on the app's tauri-pilot socket (plugin 0.7.2)."""
    REQUEST_ID[0] += 1
    line = json.dumps({"jsonrpc": "2.0", "id": REQUEST_ID[0], "method": req["method"],
                       "params": req.get("params") or {}}).encode() + b"\n"
    path = State.socket or socket_path(os.environ)
    if WINDOWS:
        deadline = time.time() + 5
        while True:
            try:
                pipe = open(path, "r+b", buffering=0)
                break
            except OSError as error:
                # 231: every instance of the pipe is busy; 2: not created yet.
                if time.time() > deadline or getattr(error, "winerror", None) not in (2, 231):
                    return {"error": {"code": "NO_PILOT", "message": "pilot pipe: %s" % error}}
                time.sleep(0.1)
        with pipe:
            pipe.write(line)
            data = b""
            while not data.endswith(b"\n"):
                chunk = pipe.read(65536)
                if not chunk:
                    break
                data += chunk
    else:
        try:
            conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            conn.settimeout(req.get("timeout", 30))
            conn.connect(path)
        except OSError as error:
            return {"error": {"code": "NO_PILOT", "message": "pilot socket %s: %s" % (path, error)}}
        with conn:
            conn.sendall(line)
            data = b""
            while not data.endswith(b"\n"):
                chunk = conn.recv(65536)
                if not chunk:
                    break
                data += chunk
    if not data:
        return {"error": {"code": "NO_PILOT", "message": "pilot closed the connection without answering"}}
    answer = json.loads(data)
    if "error" in answer:
        return {"error": answer["error"]}
    return {"result": answer.get("result")}


# ── the app's log ───────────────────────────────────────────────────────────


def newest_log():
    # A debug build (the only kind with pilot) logs to stdout, not to a file.
    logs = os.path.join(State.work or work_dir(), "logs")
    files = glob.glob(os.path.join(logs, "*.log")) or glob.glob(os.path.join(logs, "stdout.txt"))
    return max(files, key=os.path.getmtime) if files else None


def tail_text(path, n):
    try:
        with open(path, errors="replace") as f:
            return " | ".join(f.read().splitlines()[-n:])
    except OSError:
        return "(no output)"


def op_log(req):
    """Lines of the newest app log from byte `since`, optionally only those
    containing one of `grep` (case-insensitive). Opened and sought rather than
    stat'ed: a live log on Windows lists as 0 bytes."""
    path = newest_log()
    if not path:
        return {"size": 0, "lines": []}
    with open(path, "rb") as f:
        f.seek(0, 2)
        size = f.tell()
        start = req.get("since", 0)
        start = start if start <= size else 0
        cap = req.get("max_bytes", 4 << 20)
        f.seek(max(start, size - cap))
        text = f.read().decode(errors="replace")
    lines = text.splitlines()
    words = [w.lower() for w in req.get("grep", [])]
    if words:
        # `after` lines follow each match: a panic's message is on the next one.
        after = req.get("after", 0)
        hits = [i for i, l in enumerate(lines) if any(w in l.lower() for w in words)]
        keep = sorted({j for i in hits for j in range(i, min(i + after + 1, len(lines)))})
        lines = [lines[j] for j in keep]
    return {"size": size, "path": path, "lines": lines[-req.get("limit", 100000):]}


OPS = {"hello": op_hello, "start": op_start, "stop": op_stop, "alive": op_alive,
       "foreground": op_foreground, "hotkeys": op_hotkeys, "focus_window": op_focus_window,
       "press_keys": op_press_keys, "pilot": op_pilot, "log": op_log}


def main():
    for raw in sys.stdin:
        if not raw.strip():
            continue
        req = json.loads(raw)
        try:
            answer = {"id": req.get("id"), "ok": OPS[req["op"]](req)}
        except Exception as error:  # reported to the runner, never fatal here
            answer = {"id": req.get("id"), "err": "%s: %s" % (type(error).__name__, error)}
        sys.stdout.write(json.dumps(answer) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    # The things in_session() runs on the desktop of a Windows machine.
    if sys.argv[1:2] == ["--launch"]:
        with open(sys.argv[2]) as f:
            spec = json.load(f)
        spawn_windows(spec["exe"], dict(os.environ, **spec["env"]), spec["stdout"])
    elif sys.argv[1:2] == ["--query"]:
        out = os.path.join(work_dir(), sys.argv[2] + ".json")
        with open(out + ".in") as f:
            arg = json.load(f)
        with open(out + ".tmp", "w") as f:
            json.dump(QUERIES[sys.argv[2]](arg), f)
        os.replace(out + ".tmp", out)
    else:
        main()
