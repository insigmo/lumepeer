# e2e matrix: remote input between real machines

pytest drives four machines through tauri-pilot. Every `[guest, host]` pair in
`hosts.toml` runs the same three scenarios:

| test      | passes when                                                                                  |
|-----------|----------------------------------------------------------------------------------------------|
| `connect` | invite → dial → grant works and the host's picture reaches the guest's view window           |
| `mouse`   | a pointer move of 10 host pixels to the right in the guest's view moves the host's cursor by 10 ±1 px right and 0 ±1 px down |
| `keys`    | `Hello, lumepeer 42` typed in the guest's view arrives as exactly that text, and 10 chords (Ctrl+A/C/V/Z, Ctrl+Shift+Z, Alt+X, Ctrl+Alt+J, Shift+Left, Ctrl+Home, Ctrl+Enter) arrive with the same modifiers |

`guest->host` means the guest controls the host. The pairs are win↔mac, win↔linux,
mac↔linux and win↔beta, in both directions.

## Running

From Git Bash on the Windows machine:

```bash
e2e/matrix/deploy.sh                          # build + install the pilot app: win beta mac linux (or a subset)
python -m pytest e2e/matrix                   # the whole matrix
python -m pytest e2e/matrix --only win,beta   # only pairs made of these machines
python -m pytest e2e/matrix -k keys           # one scenario
```

Run `deploy.sh` again after every change to the app: each machine runs the build
it was last given.

The run ends with a short report, also saved to `target/e2e/matrix/report.txt`.
Paste it into the session that is going to fix things. Each machine's full app
log sits next to it as `target/e2e/matrix/<host>.log`, and the log of an app
that crashed and was restarted mid-run as `<host>.crashed-N.log`.

```
== lumepeer e2e matrix | 2026-09-23 23:37 | 614f8b0+dirty ==
hosts: beta=ok(Windows 10.0.26200.9550, 1920x1080@1) linux=ok(Debian GNU/Linux 13 (trixie) x86_64 wayland, 1920x1080@1) win=ok(...)
win->linux   connect FAIL  session_grant: STATE_POISONED session state is unavailable | linux log: PANIC crates/media/src/capture/linux_wayland.rs:444:25: Cannot start a runtime from within a runtime. ...
win->beta    mouse   PASS  dx=9 dy=0 (center landed (960, 540) want~(960, 540))
beta->win    keys    PASS  text ok, 9 chords ok | untested, global hotkeys of another program on win: Ctrl+Shift+Z
```

How to read it:

- `connect`: after `PASS` comes the dial time and what `connection_stats` says
  (path, transport, rtt, codec). A failure carries the guest's `connect_status`,
  the host's own flags for the session, errors from the guest view's console,
  and the last distinct WARN/ERROR lines of both logs. A panic always comes first.
- `mouse`: `dx`/`dy` is how far the host's cursor moved, in physical pixels.
  `center landed` is where the first move put it, next to where the center of
  the host's screen is.
- `keys`: `Ctrl+Alt+J(saw A)` means J reached the host with Alt held but no
  Ctrl, and `(saw nothing)` means the key never arrived. The `guest sent` part
  counts the `input_press` IPC calls the view made and how many succeeded, so
  a loss before the network shows up there. `untested, global hotkeys of
  another program` lists chords some other program on a Windows host holds
  through `RegisterHotKey`: their keydown reaches that program and no window,
  so they are left out rather than failed. `keyboard is with 'Windows
  Security' (PickerHost.exe)` names the window that has the host's keyboard
  instead of the app, here a firewall prompt; the page's own `hasFocus()`
  says true under it.
- `SKIP no session` means `connect` failed for that pair, `SKIP <host> is down`
  means that machine never came up; the `hosts:` line says why.

## What each machine needs

- **All of them:** somebody logged in at the screen (capture needs a real
  desktop session) and python3 for `agent.py` (stdlib only). pytest itself runs
  on this machine and needs python 3.11+.
- **win** (this machine): nothing else. The e2e app runs beside the installed,
  elevated client (`LUMEPEER_ALLOW_SECOND_INSTANCE`, its own file keystore,
  `RunAsInvoker`), and so, unlike the shipped client, it is not elevated.
- **beta** (Windows, `bberb@beta`): the app is started elevated in the logged-in
  session by the `LumepeerE2E` scheduled task, since sshd's session 0 has no
  desktop. `LumepeerE2EClick` makes the one click the keys test needs there,
  and `LumepeerE2EQuery` asks that desktop which window has the keyboard. The
  first time a freshly deployed build listens, Windows Defender Firewall asks
  whether to allow it (beta's Wi-Fi is a *Public* network). Until somebody
  answers at the screen, that prompt holds the keyboard.
- **mac** (`betal@betals-mac`): the Xcode Command Line Tools, rustup, node,
  cmake, and a `~/lumepeer-env.sh` that puts them on the PATH (deploy.sh
  sources it). All of it was installed on 2026-09-24 without sudo: the Command
  Line Tools by `softwareupdate -i "Command Line Tools for Xcode <version>"`,
  which an admin account may run without a password (touch
  `/tmp/.com.apple.dt.CommandLineTools.installondemand.in-progress` first so
  `softwareupdate -l` lists them), and rustup, node and cmake into the home
  directory (`~/.cargo`, `~/.local/node`, `~/.local/cmake`). deploy.sh signs
  the app as **Lumepeer E2E** (`io.insigmo.lumepeer.e2e`) with a self-signed
  certificate kept in `~/Library/Keychains/lumepeer-e2e.keychain-db`, so it is
  not confused with `/Applications/Lumepeer.app` and a grant survives rebuilds.
  Grant *Lumepeer E2E* **Screen Recording** and **Accessibility** at the Mac
  once; again only if that keychain is deleted.
- **linux** (`beta@debian`, KDE Wayland): as a host it asks for screen-share and
  remote-control permission through the portal **on every session**
  (`PersistMode::DoNot`), so someone has to click *Share* on the VM while the
  run waits (up to `wayland_picture_timeout`). Built in WSL Debian, since the VM
  has no toolchain.

## How it works

- `agent.py` runs on each machine, locally or over `ssh -T`. It starts and stops
  the app in the graphical session and relays JSON-RPC to the app's tauri-pilot
  socket (a named pipe on Windows).
- The e2e build is a debug build with `--features pilot` and the frontend
  embedded, so view windows get their IPC. Its CSP also allows `'unsafe-eval'`,
  because tauri-pilot's `eval` compiles scripts with `new Function`, which
  WebKit refuses otherwise (`pilot_config.py`; tauri.conf.json is not touched).
- The **tracker** is a textarea laid over the host app's main window. It records
  every key event that reaches that machine and keeps the text typed into it.
  The host's cursor comes from Tauri's `cursor_position`, in physical pixels. On
  Wayland, which has no global cursor position, the tracker's own pointer events
  stand in, and the main window is maximized so the pointer is over it.
- The tracker needs the host's real keyboard focus. tauri-pilot raises the
  window. On Windows that does not reach into WebView2, so the agent clicks the
  tracker, or failing that the guest clicks it through the session.
- The **guest** acts through DOM events dispatched into its view window: pointer
  moves at computed coordinates, and keys with the `key`, `code` and modifiers
  a US keyboard produces, at typing speed. Everything after that is real. OS-level
  key injection on the guest was dropped: enigo types by the guest's current
  layout, so Russian letters came out as VK_PACKET with no keyup.
- The guest's keyboard grab (ADR 0090/0107) is released for the keys test. It
  sends Ctrl/Alt/Shift from an OS hook and drops the webview's copies, and
  synthetic events never pass that hook. The hook ignores injected keys anyway,
  so it cannot be tested automatically.
- A **spy** in the guest's view counts every `input_*` IPC call and its outcome,
  via `fetch`, the transport Tauri's IPC uses.
- Before each pair, a machine whose app died or panicked (a poisoned lock
  refuses every later call) is restarted, so one crash fails one pair only.
- Win/Cmd chords are not tested: the Windows shell and the macOS menu bar take
  them before any webview sees them.
