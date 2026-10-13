# ADR 0155 — A Mac shows each change when it happens, and restarts into its update

Status: accepted
Date: 2026-10-13

Builds on [ADR 0119](0119-sessions-end-come-back-and-type-where-the-person-left-them.md)
(§4, Linux restarts into an installed update) and
[ADR 0154](0154-the-pointer-lands-where-it-points-in-every-layout.md) (§5, the
guest's decoder held a Mac's pictures back).

## Context

Reported 2026-10-13, connected to the Mac (the `macos_new` VM on `beta`): the
picture lags and does not refresh until something else happens. Typing `1`
into the address bar shows nothing; typing `2` makes the `1` appear; typing
`3` brings the `2` and the `3`. Sometimes it hangs for long, sometimes less.
Only the Mac does this. The owner also asked that the Mac restart by itself
when Lumepeer installs an update.

**The picture.** The Mac ran v0.0.142. ADR 0154 §5 had found the mechanism the
evening before and v0.0.143 carried its fix: VideoToolbox's SPS has a VUI
without `bitstream_restriction`, a decoder told nothing about reordering
assumes the worst and holds pictures back until later ones push them out, and
the encode loop now rewrites the keyframe SPS to say `max_num_reorder_frames`
0. What was not measured was how far back, and whether the guest's own
decoder honours the rewrite.

Measured 2026-10-13 with the bitstream of the Mac in the report: a clip of 24
frames, one more "character" each, encoded on the VM by the product's own
encoder selection (`Hardware`, VideoToolbox, Main, 1280x720), fed one frame
every 250 ms to a WebCodecs `VideoDecoder` (`optimizeForLatency: true`) in
the Windows guest's WebView2, counting pictures handed out after each frame:

| stream | frames not yet shown after frame 1, 2, 3, 4, 5, ... 24 |
| --- | --- |
| as v0.0.142 sent it | 1, 2, 3, 4, 5, 5, 5 ... 5 |
| SPS rewritten (v0.0.143) | 0, 0, 0 ... 0 |

So on v0.0.142 the guest showed a Mac's screen five changes late, which on a
still screen means never. A desktop that keeps changing a few times a second
(a clock, a blinking caret) pushes the held pictures out a few hundred
milliseconds late; a still one holds them until the next keystroke. That is
the report, both the "1 shows when 2 is pressed" and the "sometimes more,
sometimes less". Windows and Linux hosts were never affected: their encoders
already say `max_num_reorder_frames` 0.

**The update.** `update_install` restarts the app after a successful install
only on Linux (ADR 0119 §4). On macOS `tauri-plugin-updater` replaces the app
bundle in place and returns, and the old process kept running the old
version until somebody quit it, exactly as on Linux before ADR 0119.

## Decision

1. Nothing more in the picture path: v0.0.143 already carries the fix, and the
   Mac in the report has to be updated to it. A new e2e scenario keeps it
   fixed: `still` (3d) paints the host's tracker another colour on an
   otherwise still screen, one change at a time with 1.5 s between them, and
   the guest's view watches the picture in the page for the new colour. Each
   change has to show by itself, within the network's one-way delay plus
   250 ms; a picture held for a later one never shows at all and fails it.
2. macOS restarts into an installed update the way Linux does:
   `request_restart` after the install, which lets the exit run first, so the
   single-instance lock is free when the new process starts. Tauri's restart
   reads the new bundle's `Info.plist` for its executable, so a renamed binary
   would still be found.

## Verification

- `still`, win → beta (a Windows host, whose SPS always said it): 6 of 6
  changes shown by themselves, 127/137/192 ms min/p50/max after they were
  painted, at a 114 ms round trip.
- `still`, win → mac, run once the owner had unlocked the Mac: 6 of 6
  changes shown by themselves, 199/229/258 ms (a second run 191/217/296 ms).
  The first run, on the plain debug e2e build, also showed all six but
  301-959 ms late: unoptimised, the Mac's picture path took 110-170 ms a
  frame to convert and encode (19 ms in the release), the speed controller
  scaled the picture down, and the debug scaler added ~400 ms. That measured
  the build, not the product, so `deploy.sh mac` now builds every dependency
  and `lumepeer-media` / `lumepeer-runtime` optimised (`mac-build.toml`), two
  jobs at a time: an optimised build with four left the 8 GB VM answering
  nothing until it was reset.
- The restart, on the Mac VM: a pilot build of this change at version 0.0.1,
  started from its own folder beside the installed client (stopped for the
  run, started again after), was asked for `update_install`. It logged
  `installing an update version=0.0.143`, `update installed`, `restarting into
  the installed update`, and 1 s later a new process ran from the same path,
  whose bundle now said 0.0.143. The login item and its first-launch marker
  were unchanged.

## Consequences

- A Mac host updated to v0.0.143 or later sends a stream the guest shows as it
  decodes it.
- A Mac host still on v0.0.142 or older keeps the lag until it is updated:
  the guest does not rewrite what an old host sends.
- Pressing "install" on a Mac now restarts Lumepeer into the new version, and
  a guest connected at the time is told so first and redials (ADR 0148). The
  restart is the installing process's doing, so it starts with the first
  update installed *from* a release carrying this change: the one update that
  brings it still has to be followed by a quit and a start by hand.
