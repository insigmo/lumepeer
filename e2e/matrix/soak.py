"""A long session between two machines of the e2e matrix, watched (ADR 0119).

    python e2e/matrix/soak.py --guest win --host beta --minutes 30

Connects `guest` to `host` the way the matrix does, then every `--every`
seconds records what both ends say about the session: the host's encode
target (bitrate, frame rate) and the guest's view (picture size, status
overlay, link). A pinned preset keeps the target still for the whole session,
through any redial or resume; a target that moves, a picture that changes
size, or a view stuck on "reconnecting" is what the report names. Snapshots
of the picture at the start, the middle and the end go next to the report,
with a sharpness figure for each, so "the quality is the same after half an
hour" is a number rather than an impression.

Writes target/e2e/matrix/soak.txt and soak-<minute>.png.
"""

import argparse
import base64
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import OUT, VIEW_STATE_JS, Host, HostError, Refused, connect, disconnect, load_config  # noqa: E402

SNAPSHOT_JS = "document.getElementById('screen').toDataURL('image/png')"
# Mean absolute difference between horizontal neighbours of the picture's
# luminance: a soft picture has small steps, a sharp one large ones. Computed
# in the page, on the canvas the guest actually drew.
SHARPNESS_JS = """(() => {
  const c = document.getElementById('screen');
  if (!c || c.width < 2) return null;
  const t = document.createElement('canvas'); t.width = c.width; t.height = c.height;
  const x = t.getContext('2d'); x.drawImage(c, 0, 0);
  const d = x.getImageData(0, 0, t.width, t.height).data;
  let sum = 0, n = 0;
  for (let y = 0; y < t.height; y += 4) {
    for (let i = (y * t.width) * 4; i < (y * t.width + t.width - 1) * 4; i += 4) {
      const a = 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2];
      const b = 0.299 * d[i + 4] + 0.587 * d[i + 5] + 0.114 * d[i + 6];
      sum += Math.abs(a - b); n++;
    }
  }
  return Math.round(1000 * sum / n) / 1000;
})()"""


def stats_of(machine):
    try:
        rows = machine.ipc("connection_stats") or []
    except (HostError, Refused):
        return {}
    return rows[0] if rows else {}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--guest", default="win")
    parser.add_argument("--host", default="beta")
    parser.add_argument("--minutes", type=float, default=30)
    parser.add_argument("--every", type=float, default=30)
    args = parser.parse_args()

    cfg = load_config()
    guest, host = Host(args.guest, cfg["hosts"][args.guest]), Host(args.host, cfg["hosts"][args.host])
    rust_log = cfg["matrix"].get("rust_log", "info")
    guest.start(rust_log)
    host.start(rust_log)
    s = connect(guest, host, cfg["matrix"]["picture_timeout"])
    if s.connect_error or s.picture_error:
        sys.exit(f"no session: {s.connect_error or s.picture_error}")
    OUT.mkdir(parents=True, exist_ok=True)
    lines = [f"soak {guest.name}->{host.name} for {args.minutes} min, path {s.path}"]
    print(lines[0], flush=True)

    start = time.monotonic()
    snaps = {0: None, round(args.minutes / 2): None, round(args.minutes): None}
    targets, sizes, bad = set(), set(), []
    while True:
        minute = (time.monotonic() - start) / 60
        view = guest.js(VIEW_STATE_JS, window=s.view) if s.view in guest.windows() else None
        hs, gs = stats_of(host), stats_of(guest)
        target = (hs.get("bitrate_kbps"), hs.get("fps"))
        size = (view or {}).get("w"), (view or {}).get("h")
        overlay = (view or {}).get("overlay") or ""
        targets.add(target)
        sizes.add(size)
        if view is None or overlay:
            bad.append(f"{minute:5.1f} min: view {'gone' if view is None else repr(overlay)}")
        row = (f"{minute:5.1f} min  target {target[0]} kbps {target[1]} fps  picture {size[0]}x{size[1]}  "
               f"link {gs.get('path')}/{gs.get('transport')} rtt {gs.get('rtt_ms')} ms "
               f"loss {gs.get('loss_permille')}‰ goodput {gs.get('goodput_kbps')} kbps"
               + (f"  overlay {overlay!r}" if overlay else ""))
        for mark in snaps:
            if snaps[mark] is None and minute >= mark and view:
                try:
                    url = guest.js(SNAPSHOT_JS, window=s.view)
                    (OUT / f"soak-{mark}.png").write_bytes(base64.b64decode(url.split(",", 1)[1]))
                    snaps[mark] = guest.js(SHARPNESS_JS, window=s.view)
                    row += f"  snapshot soak-{mark}.png sharpness {snaps[mark]}"
                except (HostError, Refused, IndexError) as error:
                    row += f"  snapshot failed ({error})"
        lines.append(row)
        print(row, flush=True)
        if minute >= args.minutes:
            break
        time.sleep(args.every)

    redials = host.log_lines(grep=["media connection accepted"], limit=1000)
    resumes = host.log_lines(grep=["resumed inside its window"], limit=1000)
    moved = [t for t in targets if t != (None, None)]
    lines += [
        "",
        f"encode targets seen: {sorted(moved)} ({'steady' if len(moved) <= 1 else 'MOVED'})",
        f"picture sizes seen: {sorted(sizes)}",
        f"media connections the host accepted: {len(redials)}, resumes: {len(resumes)}",
        f"sharpness at start/middle/end: {[snaps[m] for m in sorted(snaps)]}",
        "view problems: " + ("; ".join(bad) if bad else "none"),
    ]
    (OUT / "soak.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines[-6:]), flush=True)
    disconnect(s)
    guest.stop()
    host.stop()


if __name__ == "__main__":
    main()
