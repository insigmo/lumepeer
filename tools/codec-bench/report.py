#!/usr/bin/env python3
"""Turns codec-bench runs into the tables and charts of docs/research/software-av1.md.

  python report.py --root C:/Users/beta_win/av1-research --assets ../../docs/research/software-av1 > tables.md

Reads <root>/runs-<machine>/<clip>/<tag>-<kbps>.json (timing, every machine)
and .quality.json (quality, scored once from this machine's bitstreams — the
encoders are deterministic, see the byte-identical openh264 runs on two
machines). BD-rate is Bjontegaard's measure with the piecewise-cubic (PCHIP)
interpolation current codec test conditions use instead of VCEG-M33's single
cubic, which swings wildly when two curves overlap only a little: log-rate as
a monotone function of quality over each curve's rate-quality envelope,
averaged over the quality range both curves cover.
"""
import argparse
import glob
import json
import math
import os
import sys

import numpy as np

REFERENCE = "openh264"


def load(root, machine, clip):
    rows = []
    for path in sorted(glob.glob(os.path.join(root, f"runs-{machine}", clip, "*.json"))):
        if path.endswith((".quality.json", ".vmaf.json")):
            continue
        name = os.path.basename(path)[:-5]
        tag, kbps = name.rsplit("-", 1)
        with open(path) as f:
            run = json.load(f)
        q = None
        qpath = path[:-5] + ".quality.json"
        if os.path.exists(qpath):
            with open(qpath) as f:
                q = json.load(f)
        rows.append({"tag": tag, "target": int(kbps), "run": run, "q": q})
    return rows


def curve(rows, tag, metric, segment=None):
    """(rate, quality) points of one encoder, upper envelope by rate."""
    pts = []
    for r in rows:
        if r["tag"] != tag or not r["q"]:
            continue
        src = r["q"]["segments"].get(segment) if segment else r["q"]["all"]
        if src:
            pts.append((r["q"]["kbps_actual"], src[metric]))
    pts.sort()
    env = []
    for rate, q in pts:
        if not env or q > env[-1][1] + 1e-9:
            env.append((rate, q))
    return env


def _edge_slope(h0, h1, d0, d1):
    m = ((2 * h0 + h1) * d0 - h0 * d1) / (h0 + h1)
    if np.sign(m) != np.sign(d0):
        return 0.0
    if np.sign(d0) != np.sign(d1) and abs(m) > abs(3 * d0):
        return 3 * d0
    return m


def pchip(x, y, xs):
    """Fritsch-Carlson monotone cubic through (x, y), evaluated at xs."""
    x, y = np.asarray(x, float), np.asarray(y, float)
    h, d = np.diff(x), np.diff(y) / np.diff(x)
    n = len(x)
    m = np.full(n, d[0])
    if n > 2:
        for k in range(1, n - 1):
            if d[k - 1] * d[k] <= 0:
                m[k] = 0.0
            else:
                w1, w2 = 2 * h[k] + h[k - 1], h[k] + 2 * h[k - 1]
                m[k] = (w1 + w2) / (w1 / d[k - 1] + w2 / d[k])
        m[0] = _edge_slope(h[0], h[1], d[0], d[1])
        m[-1] = _edge_slope(h[-1], h[-2], d[-1], d[-2])
    i = np.clip(np.searchsorted(x, xs) - 1, 0, n - 2)
    t = (xs - x[i]) / h[i]
    return ((2 * t**3 - 3 * t**2 + 1) * y[i] + (t**3 - 2 * t**2 + t) * h[i] * m[i]
            + (-2 * t**3 + 3 * t**2) * y[i + 1] + (t**3 - t**2) * h[i] * m[i + 1])


def _avg(x, y, lo, hi):
    xs = np.linspace(lo, hi, 2001)
    return np.trapezoid(pchip(x, y, xs), xs) / (hi - lo)


def bd_rate(ref, test):
    """Average bitrate difference (%) at equal quality; None without overlap."""
    if len(ref) < 2 or len(test) < 2:
        return None
    rq, rr = np.array([p[1] for p in ref]), np.log10([p[0] for p in ref])
    tq, tr = np.array([p[1] for p in test]), np.log10([p[0] for p in test])
    lo, hi = max(rq.min(), tq.min()), min(rq.max(), tq.max())
    if hi - lo <= 1e-6:
        return None
    diff = _avg(tq, tr, lo, hi) - _avg(rq, rr, lo, hi)
    return {"bd_rate_pct": (10 ** diff - 1) * 100, "q_lo": lo, "q_hi": hi}


def bd_quality(ref, test):
    """Average quality difference at equal bitrate; None without rate overlap."""
    if len(ref) < 2 or len(test) < 2:
        return None
    rr, rq = np.log10([p[0] for p in ref]), np.array([p[1] for p in ref])
    tr, tq = np.log10([p[0] for p in test]), np.array([p[1] for p in test])
    lo, hi = max(rr.min(), tr.min()), min(rr.max(), tr.max())
    if hi - lo <= 1e-6:
        return None
    return {"bd_q": _avg(tr, tq, lo, hi) - _avg(rr, rq, lo, hi),
            "kbps_lo": 10 ** lo, "kbps_hi": 10 ** hi}


def tags_of(rows):
    seen = []
    for r in rows:
        if r["tag"] not in seen:
            seen.append(r["tag"])
    return seen


def fmt(x, nd=1):
    return "—" if x is None or (isinstance(x, float) and math.isnan(x)) else f"{x:.{nd}f}"


def timing_table(rows, title, kbps=None):
    out = [f"\n#### {title}\n",
           "| encoder | target kbit/s | actual kbit/s | frame p50 ms | p95 | p99 | max | keyframe ms | CPU, cores | CPU, % of all | skipped | late |",
           "|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for r in rows:
        if kbps and r["target"] not in kbps:
            continue
        run, f = r["run"], r["run"]["frame_ms"]
        out.append(f"| {r['tag']} | {r['target']} | {run['kbps_actual']:.0f} | {f['p50']:.1f} | {f['p95']:.1f} | "
                   f"{f['p99']:.1f} | {f['max']:.0f} | {first_frame_ms(r):.0f} | {cores(run):.2f} | "
                   f"{cores(run) / run['ncpu'] * 100:.1f} | {run['empty_frames']} | {run['late_frames']} |")
    return "\n".join(out)


def cores(run):
    """Cores' worth of CPU per second of video, not per second of wall time: a
    disk stall stretches a run's wall time (beta cannot cache both clips) while
    every frame is still encoded exactly once."""
    media_s = run["frames"] / run["fps"]
    return (run["cpu_s"] - run["reader_cpu_s"]) / media_s


def first_frame_ms(r):
    return float("nan") if "first_ms" not in r else r["first_ms"]


def attach_first_frame(root, machine, clip, rows):
    """Frame 0 is the keyframe plus encoder warm-up: read it from the CSV."""
    for r in rows:
        path = os.path.join(root, f"runs-{machine}", clip, f"{r['tag']}-{r['target']}.csv")
        try:
            with open(path) as f:
                f.readline()
                first = f.readline().split(",")
            r["first_ms"] = float(first[3]) + float(first[4])
        except (OSError, IndexError, ValueError):
            r["first_ms"] = float("nan")


def quality_table(rows, title, segments):
    seg_names = list(segments)
    head = ("| encoder | target | actual kbit/s | dropped frames | VMAF | VMAF 5% low | PSNR-Y dB | SSIM |"
            + "".join(f" VMAF {s} |" for s in seg_names))
    out = [f"\n#### {title}\n", head, "|" + "---|" * (8 + len(seg_names))]
    for r in rows:
        if not r["q"]:
            continue
        a = r["q"]["all"]
        dropped = r["q"]["repeated"] / max(1, r["q"]["frames_scored"]) * 100
        segs = "".join(f" {r['q']['segments'].get(s, {}).get('vmaf', float('nan')):.1f} |" for s in seg_names)
        out.append(f"| {r['tag']} | {r['target']} | {r['q']['kbps_actual']:.0f} | {dropped:.0f}% | {a['vmaf']:.2f} | {a['vmaf_p5']:.1f} | "
                   f"{a['psnr_y']:.2f} | {a['ssim']:.4f} |{segs}")
    return "\n".join(out)


def bd_table(rows, title, segments, reference=REFERENCE):
    tags = [t for t in tags_of(rows) if t != reference and not t.endswith("-noasm")]
    out = [f"\n#### {title}\n",
           "| encoder | BD-rate VMAF | BD-rate PSNR-Y | BD-rate SSIM | BD-VMAF at equal rate | BD-PSNR-Y dB | VMAF overlap |",
           "|---|---|---|---|---|---|---|"]
    result = {}
    for tag in tags:
        cells = []
        res = {}
        for metric in ("vmaf", "psnr_y", "ssim"):
            b = bd_rate(curve(rows, reference, metric), curve(rows, tag, metric))
            res[metric] = b
            cells.append("no overlap" if b is None else f"{b['bd_rate_pct']:+.1f}%")
        qv = bd_quality(curve(rows, reference, "vmaf"), curve(rows, tag, "vmaf"))
        qp = bd_quality(curve(rows, reference, "psnr_y"), curve(rows, tag, "psnr_y"))
        res["bd_vmaf"], res["bd_psnr"] = qv, qp
        ov = res["vmaf"]
        overlap = "—" if ov is None else f"{ov['q_lo']:.1f}–{ov['q_hi']:.1f}"
        out.append(f"| {tag} | {cells[0]} | {cells[1]} | {cells[2]} | "
                   f"{'—' if qv is None else format(qv['bd_q'], '+.2f')} | {'—' if qp is None else format(qp['bd_q'], '+.2f')} | {overlap} |")
        result[tag] = res
    return "\n".join(out), result


# ---------------------------------------------------------------- SVG charts

SURFACE, INK, INK2, MUTED, GRID, AXIS = "#fcfcfb", "#0b0b0b", "#52514e", "#898781", "#e1e0d9", "#c3c2b7"
SERIES = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#4a3aa7", "#e34948"]
FONT = 'font-family="system-ui, -apple-system, Segoe UI, sans-serif"'


def esc(s):
    return str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def rd_chart(path, panels, metric_label, series_order):
    """Small multiples: one panel per clip, quality vs log bitrate, one line per encoder."""
    pw, ph, gap = 460, 320, 40
    m = {"l": 52, "r": 24, "t": 46, "b": 46}
    width = m["l"] + len(panels) * pw + (len(panels) - 1) * gap + m["r"]
    height = m["t"] + ph + m["b"] + 28
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" {FONT}>',
           f'<rect width="100%" height="100%" fill="{SURFACE}"/>']
    allq = [q for _, series in panels for pts in series.values() for _, q in pts]
    ylo, yhi = math.floor(min(allq) / 5) * 5, math.ceil(max(allq) / 5) * 5
    for i, (title, series) in enumerate(panels):
        x0 = m["l"] + i * (pw + gap)
        rates = [r for pts in series.values() for r, _ in pts]
        xlo, xhi = math.log10(min(rates) * 0.8), math.log10(max(rates) * 1.25)
        sx = lambda r: x0 + (math.log10(r) - xlo) / (xhi - xlo) * pw
        sy = lambda q: m["t"] + ph - (q - ylo) / (yhi - ylo) * ph
        out.append(f'<text x="{x0}" y="{m["t"] - 18}" font-size="14" font-weight="600" fill="{INK}">{esc(title)}</text>')
        step = 5 if yhi - ylo <= 40 else 10
        for q in range(int(ylo), int(yhi) + 1, step):
            out.append(f'<line x1="{x0}" x2="{x0 + pw}" y1="{sy(q):.1f}" y2="{sy(q):.1f}" stroke="{GRID}" stroke-width="1"/>')
            if i == 0:
                out.append(f'<text x="{x0 - 8}" y="{sy(q) + 4:.1f}" font-size="11" fill="{MUTED}" text-anchor="end">{q}</text>')
        for r in (250, 500, 1000, 2000, 4000, 8000, 16000, 32000):
            if xlo <= math.log10(r) <= xhi:
                out.append(f'<line x1="{sx(r):.1f}" x2="{sx(r):.1f}" y1="{m["t"] + ph}" y2="{m["t"] + ph + 4}" stroke="{AXIS}"/>')
                label = f"{r // 1000} M" if r >= 1000 else f"{r} k"
                out.append(f'<text x="{sx(r):.1f}" y="{m["t"] + ph + 18}" font-size="11" fill="{MUTED}" text-anchor="middle">{label}</text>')
        out.append(f'<line x1="{x0}" x2="{x0 + pw}" y1="{m["t"] + ph}" y2="{m["t"] + ph}" stroke="{AXIS}"/>')
        out.append(f'<text x="{x0 + pw / 2}" y="{m["t"] + ph + 36}" font-size="11" fill="{INK2}" text-anchor="middle">actual bitrate, bit/s (log scale)</text>')
        for name in series_order:
            pts = series.get(name)
            if not pts:
                continue
            color = SERIES[series_order.index(name)]
            d = " ".join(f"{'M' if k == 0 else 'L'}{sx(r):.1f},{sy(q):.1f}" for k, (r, q) in enumerate(pts))
            out.append(f'<path d="{d}" fill="none" stroke="{color}" stroke-width="2" stroke-linejoin="round"/>')
            for r, q in pts:
                out.append(f'<circle cx="{sx(r):.1f}" cy="{sy(q):.1f}" r="4" fill="{color}" stroke="{SURFACE}" stroke-width="2">'
                           f'<title>{esc(name)}: {r:.0f} kbit/s, {metric_label} {q:.2f}</title></circle>')
    out.append(f'<text x="14" y="{m["t"] + ph / 2}" font-size="11" fill="{INK2}" transform="rotate(-90 14 {m["t"] + ph / 2})" text-anchor="middle">{esc(metric_label)}</text>')
    lx = m["l"]
    ly = height - 10
    for k, name in enumerate(series_order):
        out.append(f'<rect x="{lx}" y="{ly - 9}" width="10" height="10" rx="2" fill="{SERIES[k]}"/>')
        out.append(f'<text x="{lx + 14}" y="{ly}" font-size="11" fill="{INK2}">{esc(name)}</text>')
        lx += 22 + 6.5 * len(name)
    out.append("</svg>")
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(out))


def bar_chart(path, title, rows, unit, refs, families, max_value=None):
    """Horizontal bars, one per configuration, coloured by encoder family, with reference lines."""
    bh, gap = 18, 6
    m = {"l": 150, "r": 70, "t": 44, "b": 50}
    w = 520
    height = m["t"] + len(rows) * (bh + gap) + m["b"]
    width = m["l"] + w + m["r"]
    vmax = max_value or max(v for _, v, _ in rows) * 1.1
    vmax = max(vmax, max(v for v, _ in refs) * 1.1 if refs else 0)
    sx = lambda v: m["l"] + min(v, vmax) / vmax * w
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" {FONT}>',
           f'<rect width="100%" height="100%" fill="{SURFACE}"/>',
           f'<text x="16" y="24" font-size="14" font-weight="600" fill="{INK}">{esc(title)}</text>']
    for k, (label, value, fam) in enumerate(rows):
        y = m["t"] + k * (bh + gap)
        color = SERIES[families.index(fam)]
        x1 = sx(value)
        out.append(f'<text x="{m["l"] - 8}" y="{y + bh - 5}" font-size="11" fill="{INK2}" text-anchor="end">{esc(label)}</text>')
        out.append(f'<path d="M{m["l"]},{y} H{x1 - 4:.1f} Q{x1:.1f},{y} {x1:.1f},{y + 4} V{y + bh - 4} Q{x1:.1f},{y + bh} {x1 - 4:.1f},{y + bh} H{m["l"]} Z" fill="{color}">'
                   f'<title>{esc(label)}: {value:.1f} {unit}</title></path>')
        shown = f"{value:.1f}" if value < vmax else f"{value:.0f} ›"
        out.append(f'<text x="{x1 + 6:.1f}" y="{y + bh - 5}" font-size="11" fill="{INK2}">{shown}</text>')
    base = m["t"] + len(rows) * (bh + gap)
    for v, label in refs:
        out.append(f'<line x1="{sx(v):.1f}" x2="{sx(v):.1f}" y1="{m["t"] - 6}" y2="{base}" stroke="{INK2}" stroke-width="1" stroke-dasharray="4 3"/>')
        out.append(f'<text x="{sx(v) + 4:.1f}" y="{m["t"] - 8}" font-size="10" fill="{INK2}">{esc(label)}</text>')
    out.append(f'<line x1="{m["l"]}" x2="{m["l"]}" y1="{m["t"] - 4}" y2="{base}" stroke="{AXIS}"/>')
    out.append(f'<text x="{m["l"] + w / 2}" y="{base + 20}" font-size="11" fill="{INK2}" text-anchor="middle">{esc(unit)}</text>')
    lx = m["l"]
    for k, fam in enumerate(families):
        out.append(f'<rect x="{lx}" y="{height - 19}" width="10" height="10" rx="2" fill="{SERIES[k]}"/>')
        out.append(f'<text x="{lx + 14}" y="{height - 10}" font-size="11" fill="{INK2}">{esc(fam)}</text>')
        lx += 24 + 6.5 * len(fam)
    out.append("</svg>")
    with open(path, "w", encoding="utf-8") as f:
        f.write("\n".join(out))


def family(tag):
    if tag.startswith("openh264") or tag.startswith("h264-"):
        return "openh264 (lumepeer)"
    if tag.startswith("aom"):
        return "libaom"
    if tag.startswith("svt"):
        return "SVT-AV1"
    return "Media Foundation (hardware)"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--root", required=True)
    p.add_argument("--assets", required=True)
    p.add_argument("--machines", default="pc,beta")
    p.add_argument("--quality-machine", default="pc")
    p.add_argument("--rd-series", default="openh264,aom-s9,svt-p11,mf-h264,mf-av1")
    p.add_argument("--segments", default="typing:0-300,scroll:300-600,move:600-900")
    p.add_argument("--timing-machine", default="beta-final")
    p.add_argument("--timing-kbps", type=int, default=4000)
    args = p.parse_args()
    os.makedirs(args.assets, exist_ok=True)
    segments = {}
    for part in filter(None, args.segments.split(",")):
        name, rng = part.split(":")
        segments[name] = tuple(int(v) for v in rng.split("-"))

    data = {"bd": {}, "timing": {}}
    clips = ("desktop", "game")
    qrows = {c: load(args.root, args.quality_machine, c) for c in clips}
    for clip in clips:
        print(quality_table(qrows[clip], f"Quality, {clip} clip (scored from {args.quality_machine} bitstreams)",
                            segments if clip == "desktop" else {}))
        table, bd = bd_table(qrows[clip], f"BD-rate against lumepeer's openh264 as shipped, {clip} clip (negative = fewer bits for the same quality)", segments)
        print(table)
        data["bd"][clip] = bd
        table, bd = bd_table(qrows[clip], f"BD-rate against openh264 without frame skipping, {clip} clip", segments, reference="h264-noskip")
        print(table)
        data["bd"][clip + "/noskip"] = bd
    for machine in args.machines.split(","):
        for clip in clips:
            rows = load(args.root, machine, clip)
            if not rows:
                continue
            attach_first_frame(args.root, machine, clip, rows)
            print(timing_table(rows, f"Encode time and CPU, {machine}, {clip} clip, 1080p30 real-time"))
            data["timing"][f"{machine}/{clip}"] = [
                {"tag": r["tag"], "target": r["target"], "kbps": r["run"]["kbps_actual"], "frame_ms": r["run"]["frame_ms"],
                 "cores": cores(r["run"]), "cpu_pct": cores(r["run"]) / r["run"]["ncpu"] * 100, "first_ms": r["first_ms"],
                 "skipped": r["run"]["empty_frames"]} for r in rows]

    order = args.rd_series.split(",")
    for metric, label in (("vmaf", "VMAF"), ("psnr_y", "PSNR-Y, dB")):
        panels = []
        for clip in clips:
            series = {t: curve(qrows[clip], t, metric) for t in order}
            panels.append((f"{clip} clip, 1080p30", {t: s for t, s in series.items() if s}))
        rd_chart(os.path.join(args.assets, f"rd-{metric}.svg"), panels, label, order)

    # Encode time on the target machine: p95 per configuration at one bitrate.
    families = ["openh264 (lumepeer)", "libaom", "SVT-AV1"]
    order_tags = ["openh264", "h264-noskip", "aom-s7", "aom-s8", "aom-s9", "aom-s10",
                  "svt-p8", "svt-p9", "svt-p10", "svt-p11", "svt-p12", "svt-p13"]
    for clip in clips:
        rows = {r["tag"]: r for r in load(args.root, args.timing_machine, clip) if r["target"] == args.timing_kbps}
        bars = [(t, rows[t]["run"]["frame_ms"]["p95"], family(t)) for t in order_tags if t in rows]
        bar_chart(os.path.join(args.assets, f"p95-{args.timing_machine}-{clip}.svg"),
                  f"{args.timing_machine.split('-')[0]}, {clip} 1080p30, {args.timing_kbps // 1000} Mbit/s target: frame time p95",
                  bars, "ms per frame (BGRA->I420 + encode), p95", [(16.7, "16.7 ms"), (33.3, "33.3 ms = 30 fps")],
                  families, max_value=70)

    with open(os.path.join(args.assets, "report-data.json"), "w") as f:
        json.dump(data, f, indent=1, default=float)


if __name__ == "__main__":
    sys.exit(main())
