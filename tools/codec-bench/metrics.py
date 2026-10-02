#!/usr/bin/env python3
"""Quality of every codec-bench run against its reference (docs/research/software-av1.md).

For each <tag>-<kbps>.json in a run directory: decode the bitstream with
ffmpeg (dav1d for AV1, ffmpeg's h264 decoder for H.264), line the decoded
pictures up with the source frames — a frame the encoder skipped (openh264's
rate control drops some) is shown to the viewer as the previous picture, so it
is scored as that — and score VMAF (v0.6.1), PSNR-Y and SSIM with libvmaf.

  python metrics.py --runs runs-pc/desktop --ref ref-desktop.yuv --frames 900

Writes <tag>-<kbps>.quality.json next to each run.
"""
import argparse
import csv
import glob
import json
import os
import subprocess
import sys

W, H = 1920, 1080
FRAME = W * H * 3 // 2


def set_size(size):
    global W, H, FRAME
    W, H = map(int, size.split("x"))
    FRAME = W * H * 3 // 2


def decoded_frames(bitstream):
    """Yields every decoded I420 picture of a bitstream, in output order."""
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error"]
    if bitstream.endswith(".h264"):
        cmd += ["-f", "h264"]
    else:
        cmd += ["-c:v", "libdav1d"]
    cmd += ["-i", bitstream, "-f", "rawvideo", "-pix_fmt", "yuv420p", "-fps_mode", "passthrough", "-"]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE)
    while True:
        buf = proc.stdout.read(FRAME)
        if len(buf) < FRAME:
            break
        yield buf
    proc.wait()


def score(base, ref, frames, segments):
    bitstream = next((base + ext for ext in (".ivf", ".h264") if os.path.exists(base + ext)), None)
    if bitstream is None:
        return None
    with open(base + ".csv") as f:
        produced = [int(r["bytes"]) > 0 for r in csv.DictReader(f)][:frames]
    log_path = base + ".vmaf.json"
    vmaf = subprocess.Popen(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
         "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", f"{W}x{H}", "-r", "30", "-i", "pipe:0",
         "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", f"{W}x{H}", "-r", "30", "-i", ref,
         "-lavfi", f"[0:v][1:v]libvmaf=log_path='{log_path.replace(os.sep, '/').replace(':', '\\:')}':log_fmt=json:"
                   "n_threads=16:feature=name=psnr|name=float_ssim",
         "-frames:v", str(frames), "-f", "null", "-"],
        stdin=subprocess.PIPE)
    dec = decoded_frames(bitstream)
    last = None
    decoded = repeated = 0
    for i in range(frames):
        if produced[i] or last is None:
            pic = next(dec, None)
            if pic is None:
                break
            last = pic
            decoded += 1
        else:
            repeated += 1
        vmaf.stdin.write(last)
    for _ in dec:
        pass
    vmaf.stdin.close()
    if vmaf.wait() != 0:
        print(f"  libvmaf failed for {base}", file=sys.stderr)
        return None

    with open(log_path) as f:
        log = json.load(f)
    per = log["frames"]

    def pooled(lo, hi):
        sel = [p["metrics"] for p in per[lo:hi]]
        if not sel:
            return {}
        v = sorted(m["vmaf"] for m in sel)
        return {
            "vmaf": sum(v) / len(v),
            "vmaf_p5": v[max(0, int(len(v) * 0.05) - 1)],
            "vmaf_min": v[0],
            "psnr_y": sum(m["psnr_y"] for m in sel) / len(sel),
            "ssim": sum(m["float_ssim"] for m in sel) / len(sel),
        }

    with open(base + ".json") as f:
        run = json.load(f)
    result = {
        "run": os.path.basename(base),
        "kbps_actual": run["kbps_actual"],
        "frames_scored": len(per),
        "decoded": decoded,
        "repeated": repeated,
        "all": pooled(0, len(per)),
        "segments": {name: pooled(lo, hi) for name, (lo, hi) in segments.items()},
    }
    with open(base + ".quality.json", "w") as f:
        json.dump(result, f, indent=1)
    os.remove(log_path)
    return result


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--runs", required=True)
    p.add_argument("--ref", required=True)
    p.add_argument("--frames", type=int, required=True)
    p.add_argument("--size", default="1920x1080")
    p.add_argument("--segments", default="", help="name:lo-hi,name:lo-hi (frame ranges)")
    p.add_argument("--force", action="store_true")
    args = p.parse_args()
    set_size(args.size)
    segments = {}
    for part in filter(None, args.segments.split(",")):
        name, rng = part.split(":")
        lo, hi = rng.split("-")
        segments[name] = (int(lo), int(hi))
    runs = sorted(glob.glob(os.path.join(args.runs, "*.json")))
    runs = [r for r in runs if not r.endswith((".quality.json", ".vmaf.json"))]
    for i, run in enumerate(runs):
        base = run[: -len(".json")]
        if os.path.exists(base + ".quality.json") and not args.force:
            continue
        r = score(base, args.ref, args.frames, segments)
        if r:
            a = r["all"]
            print(f"[{i + 1}/{len(runs)}] {r['run']:<24} {r['kbps_actual']:8.0f} kbps  VMAF {a['vmaf']:6.2f}  "
                  f"PSNR-Y {a['psnr_y']:6.2f}  SSIM {a['ssim']:.4f}  repeated {r['repeated']}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
