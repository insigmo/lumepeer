# ADR 0119 — Sessions end, come back and type where the person left them

Status: accepted
Date: 2026-09-27

Five reports from one evening of real use, win → beta over the internet (the
obfuscated transport to beta's public address, not the tailnet), and one
change for each. Narrows [ADR 0105](0105-a-view-window-outlives-the-connection-it-was-opened-for.md)
and [ADR 0112](0112-a-guest-that-leaves-on-purpose-is-not-waited-for.md),
amends [ADR 0064](0064-a-quality-preset-is-a-target-not-a-ceiling.md) and
[ADR 0114](0114-the-desktop-is-injected-by-a-system-worker.md)/[0115](0115-typing-goes-out-as-the-host-key-for-the-character.md).

## 1. A window that closes ends its session

**Report.** Closing the view window on the guest left the host believing the
window was still open: the session bar stayed up and the session had to be
ended at the host by hand.

**Cause.** The page asks for the session to end from its `onCloseRequested`
handler, and Tauri destroys the window as soon as that handler returns. The
handler did not wait for its `session_revoke`, so whether the request left the
page before the webview was torn down was a race. The guest's logs show it
lost: no "leaving a session from the view window" for a window the user had
closed, then "already connected to this host" when they tried to connect
again. The session was still running at both ends with no window. The
application-wide close handler made it worse: it *hid* every window but the
host bar, view windows included, so a page that never registered its handler
left a hidden window with a live session.

**Decision.**

- `view.ts` awaits the revoke, bounded at two seconds.
- `view_windows.rs` ends the session again when the window is `Destroyed`,
  whatever the page managed. A window the actor closed itself has no session
  left, and the call is answered with an unknown peer.
- View windows are not hidden on close; they close.
- Both paths use a new guest-only actor command, `leave_view`. `revoke`
  resolves a label to a peer and ends *whichever* session that peer has, and
  for a peer that is also this machine's guest that can be the host-side one.

**A window closed while its session is away** (ADR 0112's open item). The
guest now says goodbye: it claims the session once more, as a resume does,
and closes that connection with `CLOSE_NORMAL`, which the host reads as the
end of the session (ADR 0112). A resume dial already on its way is used for
that when it lands (`leaving`, matched by dial number); otherwise one is sent
for it. Nothing waits on it: a host that cannot be reached is holding a
session its own window will end.

## 2. Every dropped session is waited for on its own

**Report.** After a while the window said "reconnecting" and nothing brought
it back.

**Cause.** The guest was watching beta and the `debian` VM. Both links went
within fifteen seconds of each other (beta's network dropped; the VM sits
behind it). `reconnect_wait`, `parked_view` and the result check in
`on_dialed` were single slots — "this node makes one outgoing attempt at a
time". The second drop replaced the first host's wait and its parked window.
The first host's dial came back as "discarding the result of a superseded
dial", and nothing dialed it again: its window stayed on "reconnecting" with
no wait behind it and no label to close it by.

**Decision.** Waits and parked windows are per host
(`reconnect_waits`, `parked_views`). Each wait marks its own dial in flight
and remembers its number (`dial_seq`), so its result is recognised as its own
whatever the connect form shows; the form is shared and shows a wait only
while it is not busy with a connect the user started (`form_is_free_for_wait`).
A connect to another host no longer ends every wait and closes its window —
that was ADR 0105's reading of a single slot, and with several sessions open
it threw the others away.

## 3. The picture stops "breathing" after a reconnect

**Report.** After a long session the quality started to go better, worse,
better, worse — "it was fixed, but it still happens sometimes".

**Cause.** ADR 0064 fixed the flicker by pinning the whole target to the
guest's preset, and the guest names its preset (and its window size) once, at
mount. The host kept both on the encode loop's `EncodeControl`, which is
created per *media connection*. The first media redial or resume started a
new loop that had never been told: the adaptive controller ran again under a
preset nobody had changed, and the picture sharpened and softened on its own
for the rest of the session — exactly the flicker ADR 0064 removed, arriving
after the first drop, which on beta's link is minutes in.

**Decision.** The host keeps the preset and the window size per session
(`stream_caps`), applies them to every encode loop it starts for that guest,
and forgets them only when the session really ends (left on purpose, never
granted, or its resume window ran out). A preset that arrives before the media
connection is kept for it instead of being dropped.

## 4. Linux restarts into an installed update

On Windows the installer closes the app and starts the new one. On Linux the
package replaces the binary under a process that keeps running the old one.
After a successful install `update_install` now calls `request_restart` on
Linux: it lets the exit run first, so the single-instance plugin gives up its
name before the new process looks for it.

## 5. Typing into a virtual machine

**Report.** In a `VMware` guest on beta, typing went wrong; with Ctrl held
letters came out right, released it felt as if Ctrl were always down. Both
letters and shortcuts needed checking.

**Measured** with `e2e/matrix/test_vmware.py`: a full-screen xterm in raw mode
inside the VM writes every byte it is typed to a file, and the test reads it
back over ssh. Letters, capitals, digits, Ctrl/Alt chords, and letters typed
while Ctrl is held down. Before this ADR:

- Plain letters whose character the host's layout could not type went out as
  `KEYEVENTF_UNICODE` (ADR 0115's fallback), and `VMware` read the character's
  code as a scan code: `hello` arrived as F6 F9 . . PageUp. A stream of stray
  function and navigation keys is what read as a Ctrl stuck down.
- Ctrl, Alt and Shift from the guest's grab did not reach the VM at all:
  Ctrl+A arrived as `a`, Alt+B as `b`. A key pressed and never released
  auto-repeated in the VM (`AAAA…`).

**Causes.**

- Every event goes to the service's `LocalSystem` injector (ADR 0114) over a
  named pipe with **one** instance, served one client at a time, and between
  two clients the instance does not exist for a moment. The client gave up on
  a busy pipe at once and performed the event in-process instead. Keys come in
  bursts — a modifier and its key a few milliseconds apart — so the second
  routinely went the other way: out of order with the first, into an injector
  with its own idea of which keys are down, and — for a `VMware` window —
  nowhere at all.
- A virtual machine applies its own layout to the scan codes it is given.
  Mapping a character through the *host's* layout (ADR 0115) is right for an
  ordinary window and wrong for a VM, and the Unicode fallback is garbage there.
- On the guest, a view window dropped its own copy of `Ctrl`, `Alt` and
  `Shift` whenever the keyboard grab was *live*, on the assumption that the
  grab's hook had sent them (ADR 0107). A live grab whose hook does not see a
  key — measured here: the hook was installed and reported live, and not one
  keystroke reached its callback — leaves the webview's copy as the only one,
  and it was thrown away. The letter went on alone: "with Ctrl held the
  letters come out plain".

**Decision.**

- `lumepeer_service::client::inject_desktop` waits up to 250 ms for the pipe
  once the service has answered in this process, and fails at once where it
  never has; after a wait that runs out, later events skip the wait for two
  seconds. The host logs, once per change, whether input goes through the
  desktop injector or in-process.
- When the foreground window belongs to a virtual-machine console or a remote
  desktop client (`vmware.exe`, `vmware-vmx.exe`, `vmplayer.exe`, `vmrc.exe`,
  `VirtualBoxVM.exe`, `VirtualBox.exe`, `vmconnect.exe`, `mstsc.exe`,
  `msrdc.exe`, `qemu-system-*`), every key goes as the key under the guest's
  finger: the evdev code of the main block is its own set-1 scan code, and the
  `E0` block goes through the virtual key as before. Neither the host's layout
  nor `KEYEVENTF_UNICODE` is consulted.
- The view drops its copy of a shared key only when the grab really sent that
  transition (`lumepeer_guestkeys::sent_by_the_grab`: the press while the key
  is in the grab's `held`, the release once it is in `released`). A press the
  view had to send itself is released by the view too, whatever the grab
  recorded about that key earlier.

## Consequences

- Tests (`crates/runtime/src/network.rs`):
  `two_sessions_that_drop_together_both_come_back`,
  `closing_a_window_that_is_reconnecting_ends_the_session_at_the_host`,
  `the_guest_preset_survives_a_resume`; (`crates/media/src/capture/windows.rs`):
  `virtual_machine_and_remote_desktop_windows_read_scan_codes`,
  `a_key_for_a_virtual_machine_is_the_key_under_the_finger`. The existing
  reconnect tests pass unchanged.
- `e2e/matrix/test_vmware.py` (win → beta only) is the live check for §5.
  Running it needs the service on beta to run the build under test, since the
  injector is its child.
- The e2e harness's own guest shared its WebView2 folder with the installed,
  elevated client, so its pages rendered in an elevated browser process and
  UIPI dropped every click the harness made: no view ever got the keyboard.
  `hosts.toml` gives the local e2e app a WebView2 folder of its own.
- A guest typing into a VM with a layout the VM does not have gets the VM's
  letter for the key, as a physical keyboard would. That is the trade.
