# e2e matrix: remote input between real machines

pytest drives four machines through tauri-pilot. Every `[guest, host]` pair in
`hosts.toml` runs the same scenarios:

| test      | passes when                                                                                  |
|-----------|----------------------------------------------------------------------------------------------|
| `connect` | invite → dial → grant works and the host's picture reaches the guest's view window           |
| `mouse`   | a pointer move of 10 host pixels to the right in the guest's view moves the host's cursor by 10 ±1 px right and 0 ±1 px down |
| `keys`    | `Hello, lumepeer 42` typed in the guest's view arrives as exactly that text, and 10 chords (Ctrl+A/C/V/Z, Ctrl+Shift+Z, Alt+X, Ctrl+Alt+J, Shift+Left, Ctrl+Home, Ctrl+Enter) arrive with the same modifiers |
| `hotkeys` | Windows guest only: the same 10 chords pressed on the guest's own keyboard with its keyboard grab live arrive with the same modifiers, and Ctrl+A, Ctrl+C, Ctrl+End, Ctrl+V on `lumepeer` in the host's tracker leave `lumepeerlumepeer` |
| `terminal` | the guest reconnects to the host for the terminal alone, as the remembered host's terminal button does (ADR 0101); the host's shell shows its prompt before anything is typed, `echo $((4200+37))` (`set /a 4200+37` on a Windows host) typed into it shows `4237`, and Close leaves no shell running on the host |

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
- `hotkeys`: the same `(saw ...)` notation. `its grab is NOT live` means the
  guest's keyboard hook was not installed when the keys went in, and `the view
  forwarded input_press ok=N` counts what went the webview's way instead; with
  the grab live that is only Ctrl/Alt/Shift, which the app then drops.
- `terminal`: `prompt after 0.4s (...)` is how long the terminal took to show
  something with nothing typed, and the last lines it showed. `stayed blank`
  is the empty terminal a person sees; `polls brought N output bytes` next to
  it says whether the host sent nothing (0) or the window did not draw what
  it got. `drawing untested, mac: screen locked` means the output reached the
  guest's window, which was checked, but that window could not draw it: a
  page under a lock screen, or hidden, gets no animation frames, and xterm.js
  draws on those. The test ends the pair's ordinary session to reconnect, so
  it runs after the others.
- `session_grant: CORE concurrent guest limit for plan reached: 1` on a host
  whose terminal was just tested: pressing Close there ends the guest's
  session from the guest's side, and the host holds that session for its
  resume window (5 minutes, ADR 0089) as if the link had dropped. That is
  the app, not the harness. A new run starts every app afresh.
- `screen LOCKED` on the `hosts:` line: that Mac or Linux screen is locked. A
  locked host gives no picture (a Mac says its system "did not allow Lumepeer
  to record its screen"), and a locked guest draws nothing in its windows.
- `SKIP no session` means `connect` failed for that pair, `SKIP <host> is down`
  means that machine never came up; the `hosts:` line says why.

## What each machine needs

- **All of them:** somebody logged in at the screen (capture needs a real
  desktop session), the screen unlocked, and python3 for `agent.py` (stdlib
  only). pytest itself runs on this machine and needs python 3.11+.
- **win** (this machine): nothing else. The e2e app runs beside the installed,
  elevated client (`LUMEPEER_ALLOW_SECOND_INSTANCE`, its own file keystore,
  `RunAsInvoker`), and so, unlike the shipped client, it is not elevated.
  While the installed Lumepeer service is hosting this machine, the e2e app
  answers `NOT_THE_HOST` and every pair with win as the host fails to
  connect.
- **beta** (Windows, `bberb@beta`): the app is started elevated in the logged-in
  session by the `LumepeerE2E` scheduled task, since sshd's session 0 has no
  desktop. `LumepeerE2EQuery` runs what has to happen on that desktop: the
  click the keys tests need, and asking which window has the keyboard. The
  first time a freshly deployed build listens, Windows Defender Firewall asks
  whether to allow it (beta's Wi-Fi is a *Public* network). Until somebody
  answers at the screen, that prompt holds the keyboard. The installed client
  must not be running there: it holds the machine's host role and the e2e app
  then answers `NOT_THE_HOST`. And a VMware Workstation window holding its
  input grab (a click into the VM) freezes the cursor for every program, so
  no click reaches the tracker; an injected Ctrl+Alt does not release it, only
  a real one at the machine does.
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
  window. On Windows that does not reach into WebView2, so the agent puts the
  window on top and clicks the tracker, or failing that the guest clicks it
  through the session.
- The **guest** acts through DOM events dispatched into its view window: pointer
  moves at computed coordinates, and keys with the `key`, `code` and modifiers
  a US keyboard produces, at typing speed. Everything after that is real. OS-level
  key injection on the guest was dropped: enigo types by the guest's current
  layout, so Russian letters came out as VK_PACKET with no keyup.
- The guest's keyboard grab (ADR 0090/0107) is released for the keys test. It
  sends Ctrl/Alt/Shift from an OS hook and drops the webview's copies, and
  synthetic events never pass that hook.
- The hotkeys test drives that hook, which is the path a person's chord takes
  on a Windows guest. The agent clicks into the view's picture (on the host's
  tracker), then presses scan codes through `SendInput` with `dwExtraInfo` set
  to `E2E_MARK`. The hook ignores injected keys except ones carrying that mark,
  and only in a pilot build (`lumepeer-guestkeys/e2e`). While it runs, this
  machine's own keyboard belongs to the host.
- A **spy** in the guest's view counts every `input_*` IPC call and its outcome,
  via `fetch`, the transport Tauri's IPC uses.
- Before each pair, a machine whose app died or panicked (a poisoned lock
  refuses every later call) is restarted, so one crash fails one pair only.
- Win/Cmd chords are not tested: the Windows shell and the macOS menu bar take
  them before any webview sees them.
