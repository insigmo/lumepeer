# ADR 0104 — A desktop handle carries the right to send input

Status: accepted
Date: 2026-09-22

Answers the report "in UAC mode I cannot do anything"
(`docs/bugs/21-uac-and-secure-desktop.md`, user item 5). Fixes
[ADR 0057](0057-lumepeer-takes-full-local-control.md)'s input path, which had
never once performed an event, and closes the "the UAC picture is not fixed"
item left open by
[ADR 0092](0092-a-refused-duplication-is-not-proof-of-a-secure-desktop.md).

## Context

The secure-desktop feature is two halves built the same way: `secure_desktop.rs`
takes the picture, `secure_desktop_input.rs` performs the click. Both run in a
worker the service launches into the console session on `WinSta0\Winlogon`
(ADR 0049, ADR 0057). Measured on a real machine on 2026-09-22 against the
installed service, with a genuine UAC prompt raised.

**The picture half works.** Eleven consecutive frames for as long as the
prompt was up, from both ends of the pipe:

```text
  6420ms input_desktop=OpenInputDesktop failed: Access is denied.  frame=SOME 1536x864 5308416 bytes
 10937ms input_desktop=OpenInputDesktop failed: Access is denied.  frame=SOME 1536x864 5308416 bytes
 11387ms input_desktop=Default                                     frame=NONE
2026-09-22T14:39:35Z INFO secure-desktop worker: published a frame width=1536 height=864
```

ADR 0092 recorded that this had "never once succeeded" and that the reason
would come from the next reproduction. It did, and it was the opposite of what
was expected: the capture was never broken. Every failure in the log is an
episode the worker reported as `input_desktop="Default"` — no secure desktop
there to capture.

**The input half had never worked.** Every event, every attempt:

```text
2026-09-22T14:44:10Z WARN the OS refused a secure-desktop input event sent=0 offered=1 error=Access is denied. (0x80070005)
```

Four measurements narrowed it, and three of them were dead ends worth
recording, because each is an explanation that reads well and is false:

1. **Not the secure desktop.** The same event with no UAC prompt anywhere,
   ordinary desktop in front, is refused identically. Whatever this is, it is
   not "`Winlogon` is hard to reach" — which is what every comment on this
   path assumed.
2. **Not the session.** `Win32_Process` during a storm of injections shows the
   worker in session 1, beside the service in session 0. The token is stamped
   correctly.
3. **Not the window station.** The worker is already on the interactive
   `WinSta0` — `UOI_FLAGS` answers `WSF_VISIBLE` with no switching at all — and
   adding an explicit `SetProcessWindowStation` changed nothing. That switch
   was written, measured, and removed again.
4. **Not integrity.** The worker's token is System integrity (`16384`), the
   highest there is, so UIPI has nothing to refuse.

What was left is the one thing none of the four touches: the **access mask the
desktop handle was opened with**. `attach_to_input_desktop` asked for
`DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS`. `SendInput` checks the thread's
desktop handle for **`DESKTOP_JOURNALPLAYBACK`**, and a handle without it is
refused at the last step. Two masks, same process, same desktop, microseconds
apart:

```text
DIAG ladder mask="0x81" switched=true sent=0 err=5
DIAG ladder mask="0xa1" switched=true sent=1
```

The name is historical: journal playback is the old hook-based way of feeding
synthetic input, and `SendInput` inherited its access check. Nothing here plays
back a journal.

Nothing in the failure said any of this. `OpenInputDesktop` granted the handle,
`SetThreadDesktop` moved onto it, both returned success, and the right the
handle did not carry was consulted only inside `SendInput` — which reports it
as the same `ERROR_ACCESS_DENIED` a thread on the wrong desktop gets. That
collision is the whole reason this took nine reproductions: the error code for
"you are in the wrong place" and for "you may not do this here" is one code.

## Decision

**`attach_to_input_desktop` asks for `DESKTOP_JOURNALPLAYBACK` as well.** One
bit, on the handle the thread is about to stand on. Everything else on this
path is unchanged: same worker, same token, same session, same desktop, same
`SendInput`.

**A refusal now says whether the worker was on the desktop receiving input.**
The missing line was the whole cost of this bug — a refusal that reports
`moved_onto_it=true` rules out the entire family of "it was on the wrong
desktop" explanations at a glance, which is where four of the five days went.

## Consequences

- A full-control guest can click a UAC prompt on the host: what ADR 0057
  described and ADR 0061 granted, delivered for the first time. Until now the
  grant existed, the picture arrived, the event was authorized, dispatched,
  and refused by the OS at the last step, with one line in a log nobody had
  reason to open.
- Nothing is widened. `DESKTOP_JOURNALPLAYBACK` is a right on a desktop this
  worker already had open and was already standing on, asked for by a
  `LocalSystem` process that already holds every right on that object. The
  access check it satisfies is the one that was always meant to be satisfied
  here — ADR 0057's whole purpose is that this worker performs input. Reaching
  the worker at all still takes `secure_desktop_input`, which only a
  full-control session carries (ADR 0061).
- ADR 0092's fall-through to the ordinary injector stops firing on every
  genuine secure-desktop event. It was never masking this: on a real secure
  desktop the in-session injector is refused too, so the event died either
  way. It keeps doing its job for the episodes that were never secure.
- Tests: none that a test harness can express. The behaviour being fixed is
  which rights a `LocalSystem` process in another session asked for on a
  desktop object, and the two outcomes differ only inside `SendInput`. It is
  verified the way it was diagnosed — a reproduction against the installed
  service — and belongs in `docs/release-checklist.md`, where ADR 0092 put the
  injection path for the same reason.

## Still open

- **`SecureDesktopActive` is still asserted without evidence.** ADR 0092 named
  this and fixed only its effect on input. The host log now proves the rest:
  all twelve episodes this machine recorded over 2026-09-21/22 carry
  `input_desktop="Default"`, so the capturer called a secure desktop something
  that was a full-screen application, a mode change or a driver reset every
  time. While that lasts the guest's pointer moves are cached and never sent
  (`network.rs`), and the picture is replaced by a `Winlogon` snapshot of a
  desktop that is not the one in front. That is
  `docs/bugs/21-uac-and-secure-desktop.md` task 3, and it is its own change.
- The worker still spends a process per event. That is ADR 0057's shape and
  this changes nothing about it.
