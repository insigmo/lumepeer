# ADR 0158 — A host lets go of every encoder, and a Mac looks like a Mac

Status: accepted
Date: 2026-10-14

Builds on [ADR 0143](0143-the-guest-can-pick-the-hosts-encoder.md) (the
guest's encoder picker), [ADR 0090](0090-the-keys-a-view-window-never-sees.md)
(the view window's own hotkeys) and
[ADR 0128](0128-the-mac-release-is-signed-and-keeps-its-secrets-in-files.md)
(the signed Mac release whose permissions survive updates).

## Context

Reported 2026-10-14 by the owner after a session in which a MacBook guest
watched this Windows laptop (NVIDIA + Intel graphics) as its host:

1. The view window's hotkeys are a PC keyboard's (`Ctrl+Alt+Shift+…`).
2. Hardware AV1 "was not chosen by default".
3. Lumepeer took 3-4 GB of memory with the picture on.
4. The Mac's tray icon is a square.
5. The Mac installer shows an extra icon.
6. The main window's minimize/maximize/close are Windows' on a Mac.
7. The Mac should hold every permission Lumepeer asks for from the start.

What the host showed, before any code was read:

- **Memory.** Twelve minutes after the session ended the host process still
  held 3.3 GB working set, 7 GB private and **1,289 threads**. Its log
  recorded the guest switching encoders about 16 times — each switch is a
  new encoder. A throwaway loop that built the real encoder selection,
  encoded 30 frames at 3420x1922 and dropped it, eight rounds of AV1 then
  H.264, reproduced it with nothing else running: **+60 threads and
  +400 MB per AV1 encoder, +6 threads and +180 MB per H.264 one**, 524
  threads and 4.1 GB after 16 encoders.
- **AV1.** The host's log for the same session says `video codec negotiated
  codec=Av1 encoder=Auto` and `hardware encoder available, using Media
  Foundation codec=Av1`: "Automatic" had picked hardware AV1. The guest's
  picker said only "Automatic", which read as AV1 not being chosen.
- **Tray.** The tray used the app icon — an opaque navy tile — as a macOS
  template image. A template image is drawn from its alpha alone, and an
  opaque tile's alpha is a square.
- **Installer.** The extra item is a dimmed copy of the raw, square app icon
  in the first free slot below the laid-out pair: the volume icon file
  `.VolumeIcon.icns` Tauri's DMG bundler puts in every image. It is hidden,
  and Finder shows hidden files dimmed only while "show hidden files"
  (`⌘⇧.`) is on — which this Mac has. Every DMG with its own disk icon shows
  the same.
- **Permissions.** No installer can switch on Screen & System Audio
  Recording, Accessibility or Microphone: they are TCC permissions only the
  person at the Mac gives (or a managed-device profile). The app asked for
  each at the moment a session first needed it — Screen Recording in the
  middle of a guest's first connection, Accessibility at its first click.

## Decision

**A hardware transform is shut down when its encoder lets go of it.**
`activate_hardware_transform` hands back a `TransformShutdown` with the
transform: it holds the transform and the `IMFActivate` it came from, and
on drop calls `MFShutdownObject` (`IMFShutdown::Shutdown` of an asynchronous
transform) and `IMFActivate::ShutdownObject`. Releasing the last reference
had never been enough: the activation object keeps the transform, and an
asynchronous transform keeps its event queue, worker threads and driver
session until it is shut down. The encoder holds the guard after its own
references, so the transform is shut down once nothing else holds it, and a
re-activation shuts the transform it replaces. The H.264 probe, which drops
its transform at once, is covered by the same type. Measured with the same
loop: 8 threads and 50-70 MB across all 16 encoders.

**"Automatic" says what it resolved to:** "Automatic (AV1, hardware)". The
guest judges it from the codec its picture arrives in and what the host
offered — that codec's hardware encoder when offered, its software one
otherwise, which is the host's own rule — so the protocol is unchanged.

**On a Mac the view window's hotkeys are `⌃⌥⌘`** (Control+Option+Command),
written the way macOS menus write them (`⌃⌥⌘F`). A Mac keyboard has no Alt,
an app's own chords there go through Command, and three modifiers keep the
rarity `Ctrl+Alt+Shift` has elsewhere. Matching stays exact: `⌃⌥⌘⇧F` and
`Ctrl+Alt+Shift+F` both go to the remote machine on a Mac. Nothing else
claims the chord first: the keyboard grab of ADR 0090 is Windows-only.

**The Mac's tray icon is a template of the glyph alone:**
`icons/tray-template.png`, the app icon's infinity sign cut out of its tile,
black on transparent, 18 pt tall at 2x. Windows and Linux keep the app icon.

**A Mac keeps its own window controls.** `tauri.macos.conf.json` (merged
over `tauri.conf.json` on macOS only) gives the main window decorations with
the `Overlay` title bar, a hidden title and the traffic lights at (18, 22),
centred on the sidebar's brand row. The webview draws no controls of its
own there, and the brand row moves right of the lights. Windows and Linux
are unchanged.

**A Mac asks for its three permissions before anyone connects.** A
permissions sheet lists Screen & System Audio Recording, Accessibility and
Microphone, each with where it stands and, while off, the one button that
asks for it: the system prompt where it still has one, and System Settings
opened at the permission's list. It reads the three again every 1.5 s while
open. It opens by itself on the first start, and again when a permission it
saw on has gone (an update signed differently loses them); the settings
reopen it. `lumepeer_media::capture::macos_permissions` does the asking —
the desktop crate forbids `unsafe`, and the microphone's state is
`AVCaptureDevice`'s, reached through the Objective-C runtime.

**The installer is left as it is.** Hiding `.VolumeIcon.icns` from a Finder
that shows hidden files would mean dropping the DMG's own disk icon; with
hidden files off (the default) the window holds only Lumepeer and
Applications.

## Consequences

- A guest can switch encoders as often as it likes: the host's threads and
  memory no longer grow with each switch. A residue of about nine handles an
  encoder remains (Media Foundation's own start and shutdown); it does not
  grow threads or memory.
- `a_dropped_encoder_gives_its_handles_back` (ignored; it counts the whole
  process's handles, so it runs alone with `--test-threads=1`) failed before
  the fix — 504 handles before four H.264 encoders, 792 after — and passes
  after it.
- The guest's "Automatic" label is a judgement from the codec, not the
  host's word: a host whose hardware encoder failed after the choice would
  still be named "hardware" until the next stream.

## Not verified

- On a real Mac: the traffic lights' position against the brand row, the
  tray glyph in a light and a dark menu bar, and the permissions sheet
  driving the real prompts. The Mac code compiles and passes clippy on the
  `betals-mac` VM; the sheet and the title strip were checked in a browser
  with the Mac's user agent and a mocked IPC.
- Whether `CGPreflightScreenCaptureAccess` reports a Screen Recording grant
  before the restart macOS asks for. The sheet tells the person to choose
  "Quit & Reopen" either way.
- The leak fix on the live host pair: verified by the encoder loop and the
  handle test, not by another guest session.
