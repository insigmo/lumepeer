# ADR 0036 — A two-node stand that CI brings up on every push

Status: accepted
Date: 2026-08-27
Extends: §19 (phase acceptance), §13 (the IPC surface), §2.1 (deny by
default), §2.2 (no hidden capture)
Builds on: ADR 0023 (tauri-pilot as the UI driver, the encrypted-file
keystore for headless sessions), ADR 0027 (the dial left the actor loop),
ADR 0035 (the export's own IPC commands)

## Context

The suite had two ends and nothing in between.

- Unit and actor tests are thorough and genuinely networked — `network.rs`
  spins up two real iroh endpoints and runs real handshakes between them — but
  they call the actor directly. They never cross Tauri's IPC boundary, never
  meet the ACL in `capabilities/*.json`, and never render anything.
- `e2e/smoke.toml` drives a real app through tauri-pilot, but only ever one
  app, with nobody connected to it. It can assert that a command exists and
  refuses a stranger. It cannot assert that a session works.

Everything that only exists when two apps are talking — consent moving a
session from pending to active, the four independent grants staying off
across that move, capture and encode producing a picture on a live display,
the recorder writing it, the export turning it into files — was covered by
nothing that runs on a push. It was covered by someone remembering to run a VM
by hand, which is another way of saying it was covered by nothing.

Three smaller things also turned out to be broken precisely because nothing
ran them. `e2e/smoke.toml` invoked through `window.__TAURI__.core.invoke`, and
this app does not set `withGlobalTauri` — so every IPC step in it was
asserting against a global that does not exist in its webview. Its first step
asserted the window's URL contains `index.html`, which a debug build's URL
does not. And nothing anywhere served the webview bundle a debug build goes
looking for, so the window it all ran against was blank.

## Decision

### One machine, one display, two whole apps

`e2e/ci-stand.sh` starts two debug builds of the desktop app on one Xvfb
display and drives each through its own tauri-pilot socket. What makes them
two nodes rather than one process talking to itself is that they share no
directory: each gets its own `XDG_DATA_HOME`, `XDG_CONFIG_HOME`,
`XDG_RUNTIME_DIR` and encrypted-file keystore. The runtime dir is what gives
each its own pilot socket, because the plugin names it
`$XDG_RUNTIME_DIR/tauri-pilot-<identifier>.sock` — two nodes sharing one
runtime dir would collide on one socket and the second would never come up.

The keystore is the file backend of ADR 0023 for the same reason the VM
scripts use it: a session with no logged-in desktop has no unlocked Secret
Service collection, and a stand that stops on a keyring prompt is not a stand.

### The scenario is the session, not the screen

Steps go through `__TAURI_INTERNALS__.invoke` — the same bridge the app's own
bundle calls through, so Tauri's ACL applies exactly as it does to the UI —
rather than through clicks. Clicking is what `smoke.toml` and the vitest
suites already cover; what has never been covered is the *session*, and a
scenario that spends its steps on selectors is a scenario that breaks when a
class name changes and passes when consent does not.

`smoke.toml` keeps its place: the stand runs it against the host node before
pairing, so the two scenarios cannot drift apart and the declarative half gets
run on every push too. Its IPC steps are corrected to the internals bridge.

What the stand asserts, in order: both nodes come up; an invite is issued; the
guest's dial reaches `awaiting_consent`; the host sees a **pending** session
and nothing is granted yet; the host grants a role and the guest reaches
`connected`; the four independent grants are **still off** after that (§8.2 —
a role must not carry them); the guest opened a window for the picture;
recording is **refused** without its grant; with the grant, a recording runs
and the host reports it (§2.2); the recording is listed; three names that are
paths are refused (§2.3); the export writes a file that begins with an Annex-B
start code; a revoke empties the session list; neither app died.

Half of those are negative assertions. That is deliberate: an e2e that only
proves the happy path proves that the feature works, not that the rule holds.

### The stand serves the webview bundle itself

A debug build of a Tauri app loads `build.devUrl`, not the bundle compiled
into it — `tauri-build` sets `cfg(dev)` for a debug profile and
`generate_context!` embeds the URL instead of the files. And the pilot bridge
exists **only** in a debug build, by design (main.rs gates it on
`debug_assertions` as well as on the feature). So an e2e that drives the app
through pilot must also make that URL answer, or it drives a blank window.
That is why every IPC step of the old smoke run could never have passed: the
page was never there.

The stand therefore serves `apps/desktop/dist` at the configured dev URL for
as long as it runs, reading the port out of `tauri.conf.json` so the two
cannot drift. `capabilities/main.json` already lists `http://localhost:*` as
a remote origin for the main window, which is what lets the served page reach
the IPC surface at all — the ACL is not relaxed for the stand.

### Relay reachability is reported, not required

`network_status.ready` says this node reached a relay. Two nodes on one
machine do not need one — the invite carries direct addresses and the dial is
local — so the stand prints it and moves on. A runner with no route to the
relay fleet still runs the whole scenario, and the stand fails for the reason
it actually failed rather than at a gate it did not need to pass.

### It fails the build

The `e2e` job runs on `ubuntu-latest` on every push and pull request, uploads
`target/e2e` (JUnit, both app logs, screenshots) on success or failure, and is
not `if: false`. There is a job in this workflow that is parked behind a
runner that does not exist (`resource-budget`, ADR 0008); this is not another
one. It needs a stock runner and nothing else.

`E2E_REQUIRE_VIDEO=1` is the default: a recording that carried no picture
fails the job. The alternative — accepting an empty export — would let the
whole capture and encode path rot silently while the stand stayed green.

## Consequences

- CI grows a job of roughly ten to fifteen minutes, most of it the debug build
  of the app, cached between runs.
- The stand needs `python3` (present on every runner) to read JSON answers,
  rather than `jq`, which is not guaranteed and is one more thing to install.
- A flake in the stand is a flake in the product's connection path, and should
  be read that way before it is read as a flaky test. If one appears, the
  artifacts hold both apps' logs from the failing run.
- `task e2e:stand` runs the same script on the X11 VM against a display
  someone can watch, so a CI failure can be reproduced by eye.
- The stand does not assert that the *guest* sees a picture, only that it
  opened a window for one. `capabilities/view.json` grants its commands
  `local` only, with no `remote.urls` — so in a debug build, where the webview
  is served from `devUrl`, every command a view window calls is denied by the
  ACL. That is a debug-only gap in an otherwise deliberate narrowing (the
  main window's capability does list the localhost origins), and widening
  view.json to match is a security decision for its own change, not a thing
  to do in passing to make a test greener. The host's own capture, encode and
  recording — which is what the recording steps assert — do not depend on it.
- The stand covers one platform. Windows and macOS still have no session-level
  e2e: neither has a headless display CI can drive the way Xvfb can, and both
  would need a different bootstrap (a named pipe per instance on Windows,
  where the pilot plugin derives the pipe name from the app identifier alone —
  two instances on one machine cannot each hold one).
- Wayland is not covered either. `capture-portal` needs a portal to answer,
  which means a session bus and a compositor, and on a runner that is a second
  stand rather than a flag on this one.

## Verification

- Against a live app on Windows (one node, the pilot bridge over its named
  pipe): `ipc`, `ipc_ok`, `json_field`, `session_count`, `phase_of` and
  `wait_until` all answer correctly from real payloads — `network_status`,
  an empty `session_status`, a 350-character invite, `recordings_list`, three
  refused export names, an unknown command, and `view_next_frame` refused by
  the ACL from the main window.
- `e2e/smoke.toml` passes 7/7 against that same live app, which it had never
  done before: its IPC steps called a global this app does not define, and its
  URL assertion expected a file name a debug build's URL does not carry.
- `bash -n` on the script, and its parsing helpers exercised directly against
  recorded IPC answers (`json_field` on lists, objects, booleans and missing
  keys; `session_count` and `phase_of` on both a good answer and an
  unreachable node; `wait_until` reaching its value and timing out).
- The generated eval scripts run against a stubbed `invoke` in node, and
  produce `OK:<json>` for a value, `OK:null` for a command that answers
  nothing, and `ERR:<code>` for a refusal.
- The JUnit writer produces a well-formed report for a failing run, a passing
  run, and a run that failed before its first step.
- The two-node half — both nodes up, the pairing, the live recording and its
  export — has not run green yet: it needs a Linux display (two instances
  cannot share a machine on Windows, where the pilot plugin derives one pipe
  name from the app identifier), and the Linux VM was down when this was
  written. Its first real run is the CI job it was added to.
