#!/usr/bin/env python3
"""Side-by-side crops of one frame: the captured original and each encoder's picture.

  python crops.py --clip desktop.bgra --size 1920x1080 --start 30 --frame 450
      --crop 470,330,420,170 --zoom 2 --out text-scroll.png
      "openh264 1.4 Mbit=runs/desktop/openh264-4000.h264" "SVT-AV1 p12 1.3 Mbit=runs/desktop/svt-p12-1000.ivf"

The frame number counts from --start, like the bench's. A frame the encoder
skipped is shown as the picture before it, which is what the viewer saw. The
crop is enlarged by pixel repetition, never smoothed, so the codec's own
artefacts are what is visible.
"""
import argparse
import csv
import os
import subprocess

from PIL import Image, ImageDraw, ImageFont


def decoded_index(csv_path, frame):
    with open(csv_path) as f:
        produced = [int(r["bytes"]) > 0 for r in csv.DictReader(f)]
    return max(0, sum(produced[: frame + 1]) - 1)


def decode_frame(bitstream, index, w, h):
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error"]
    cmd += ["-f", "h264"] if bitstream.endswith(".h264") else ["-c:v", "libdav1d"]
    cmd += ["-i", bitstream, "-vf", f"select=eq(n\\,{index})", "-fps_mode", "passthrough",
            "-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo", "-"]
    raw = subprocess.run(cmd, capture_output=True, check=True).stdout
    return Image.frombytes("RGB", (w, h), raw[: w * h * 3])


def original_frame(clip, w, h, index):
    with open(clip, "rb") as f:
        f.seek(index * w * h * 4)
        return Image.frombytes("RGB", (w, h), f.read(w * h * 4), "raw", "BGRX")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--clip", required=True)
    p.add_argument("--size", required=True)
    p.add_argument("--start", type=int, default=0)
    p.add_argument("--frame", type=int, required=True)
    p.add_argument("--crop", required=True, help="x,y,w,h")
    p.add_argument("--zoom", type=int, default=2)
    p.add_argument("--out", required=True)
    p.add_argument("panels", nargs="+", help="label=bitstream")
    a = p.parse_args()
    w, h = map(int, a.size.split("x"))
    x, y, cw, ch = map(int, a.crop.split(","))
    box = (x, y, x + cw, y + ch)
    tiles = [("original (captured)", original_frame(a.clip, w, h, a.start + a.frame))]
    for panel in a.panels:
        label, path = panel.split("=", 1)
        base = os.path.splitext(path)[0]
        tiles.append((label, decode_frame(path, decoded_index(base + ".csv", a.frame), w, h)))
    font = ImageFont.load_default(size=16)
    label_h = 26
    tw, th = cw * a.zoom, ch * a.zoom
    sheet = Image.new("RGB", (tw, (th + label_h) * len(tiles)), (252, 252, 251))
    draw = ImageDraw.Draw(sheet)
    for k, (label, img) in enumerate(tiles):
        top = k * (th + label_h)
        draw.text((6, top + 4), label, fill=(11, 11, 11), font=font)
        sheet.paste(img.crop(box).resize((tw, th), Image.NEAREST), (0, top + label_h))
    sheet.save(a.out, optimize=True)


if __name__ == "__main__":
    main()
