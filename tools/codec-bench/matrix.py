#!/usr/bin/env python3
"""Stage-1 run matrix for the software-AV1 study (docs/research/software-av1.md).

Writes the codec-bench command lines for one machine and one clip, either as a
script to copy to that machine (Windows .cmd or POSIX sh) or runs them here.

  python matrix.py --machine pc   --clip desktop --run
  python matrix.py --machine beta --clip game --emit cmd > run-game.cmd

Every run writes <out>/<clip>/<tag>-<kbps>.{json,csv,ivf|h264}.
"""
import argparse
import os
import subprocess
import sys

BITRATES = [2000, 4000, 8000, 16000]

# name -> (file, first frame, frame count, screen-content flag, width, height)
CLIPS = {
    "desktop": ("desktop.bgra", 30, 900, 1, 1920, 1080),
    "game": ("csgo.bgra", 0, 600, 0, 1920, 1080),
    # This PC's 4K desktop (250% scaling), area-downscaled the way a guest
    # window smaller than the host screen gets it, and the 4K original from
    # the scroll segment on.
    "desktop1440": ("desktop1440.bgra", 30, 875, 1, 2560, 1440),
    "desktop4k": ("desktop4k.bgra", 0, 575, 1, 3840, 2160),
}


def configs(machine, threads):
    """(tag, binary key, encoder args) for every encoder configuration."""
    out = [
        ("openh264", "asm", ["--encoder", "openh264"]),
        ("openh264-noasm", "noasm", ["--encoder", "openh264"]),
        # The fallback with its rate control changed instead of its codec.
        ("h264-noskip", "asm", ["--encoder", "openh264-noskip"]),
        ("h264-cbr", "asm", ["--encoder", "openh264-cbr"]),
    ]
    for speed in (7, 8, 9, 10):
        out.append((f"aom-s{speed}", "asm", ["--encoder", "aom", "--speed", str(speed),
                                             "--threads", str(threads), "--tiles", "2"]))
    # SVT-AV1 4.2 keeps its screen-content tools (palette, intra block copy)
    # only at presets 8 and below, in every mode; 9+ always takes the
    # natural-content path. 7 is left out: ~100 ms a 1080p frame.
    for preset in (8, 9, 10, 11, 12, 13):
        out.append((f"svt-p{preset}", "asm", ["--encoder", "svt", "--speed", str(preset), "--threads", "0"]))
    # Stage 2 (ADR 0139): the product's own software AV1 encoder, from its
    # own binary (`--no-default-features --features lumepeer-aom`). Its
    # `--kbps` is the session's H.264 figure and libaom gets half of it, so
    # the command below passes twice the matrix bitrate: lumepeer-aom-<T>
    # has libaom at T, exactly like aom-s10-<T>, and the two must agree.
    out.append(("lumepeer-aom", "lumepeer", ["--encoder", "lumepeer-aom"]))
    if machine == "pc":
        out.append(("mf-h264", "asm", ["--encoder", "mf-h264"]))
        out.append(("mf-av1", "asm", ["--encoder", "mf-av1"]))
    return out


def commands(args):
    name, start, frames, screen, width, height = CLIPS[args.clip]
    clip = os.path.join(args.clips, name) if args.clips else name
    out_dir = os.path.join(args.out, args.clip)
    cmds = []
    for tag, binary, enc in configs(args.machine, args.threads):
        if args.only and not any(tag.startswith(o) for o in args.only.split(",")):
            continue
        for kbps in [int(b) for b in args.bitrates.split(",")]:
            ext = "h264" if "h264" in tag else "ivf"
            base = os.path.join(out_dir, f"{tag}-{kbps}")
            exe = {"noasm": args.bin_noasm, "lumepeer": args.bin_lumepeer}.get(binary, args.bin)
            session_kbps = kbps * 2 if binary == "lumepeer" else kbps
            cmd = [exe, "encode",
                   "--input", clip, "--width", str(width), "--height", str(height),
                   "--start", str(start), "--frames", str(frames), "--fps", "30",
                   "--kbps", str(session_kbps), "--screen", str(screen), "--minq", "0", "--maxq", "63",
                   *enc, "--out", f"{base}.{ext}", "--csv", f"{base}.csv", "--json", f"{base}.json"]
            cmds.append((base, cmd))
    return out_dir, cmds


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--machine", required=True)
    p.add_argument("--clip", required=True, choices=CLIPS)
    p.add_argument("--clips", default="", help="directory holding the .bgra clips")
    p.add_argument("--out", default="runs")
    p.add_argument("--bin", default="codec-bench.exe")
    p.add_argument("--bin-noasm", default="codec-bench-noasm.exe")
    p.add_argument("--bin-lumepeer", default="codec-bench-lumepeer.exe",
                   help="built with --no-default-features --features lumepeer-aom (ADR 0139)")
    p.add_argument("--threads", type=int, default=8, help="libaom threads")
    p.add_argument("--only", default="", help="comma-separated tag prefixes")
    p.add_argument("--bitrates", default=",".join(map(str, BITRATES)),
                   help="comma-separated kbps targets (default: the 1080p set 2/4/8/16 Mbit)")
    p.add_argument("--emit", choices=["cmd", "sh"])
    p.add_argument("--run", action="store_true")
    args = p.parse_args()

    out_dir, cmds = commands(args)
    if args.emit == "cmd":
        print("@echo off\r")
        print(f'if not exist "{out_dir}" mkdir "{out_dir}"\r')
        for base, cmd in cmds:
            line = " ".join(f'"{c}"' if " " in c else c for c in cmd)
            print(f'{line} > NUL 2> "{base}.err"\r')
        print(f'echo done > "{out_dir}\\DONE"\r')
    elif args.emit == "sh":
        print("#!/bin/sh")
        print(f'mkdir -p "{out_dir}"')
        for base, cmd in cmds:
            print(" ".join(f"'{c}'" for c in cmd) + f" > /dev/null 2> '{base}.err'")
        print(f'echo done > "{out_dir}/DONE"')
    elif args.run:
        os.makedirs(out_dir, exist_ok=True)
        for i, (base, cmd) in enumerate(cmds):
            if os.path.exists(base + ".json"):
                continue
            print(f"[{i + 1}/{len(cmds)}] {os.path.basename(base)}", flush=True)
            with open(base + ".err", "w") as err:
                rc = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=err).returncode
            if rc != 0:
                print(f"  failed rc={rc}, see {base}.err", flush=True)
    else:
        for _, cmd in cmds:
            print(" ".join(cmd))


if __name__ == "__main__":
    sys.exit(main())
