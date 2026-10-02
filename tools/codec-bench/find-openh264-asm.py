#!/usr/bin/env python3
"""Does a Windows binary contain openh264's x86 assembly?

openh264-sys2 drops all of its nasm-built SIMD silently when it cannot run
nasm (docs/research/software-av1.md). This takes the machine code of a few
nasm-built objects from an asm-enabled build (COFF .text, relocated bytes
skipped) and searches a binary for it.

  python find-openh264-asm.py <dir with nasm .o files> <binary> [<binary> ...]
"""
import os
import struct
import sys

OBJECTS = ["satd_sad.o", "mc_luma.o", "quant.o", "dct.o", "vaa.o"]
WINDOW = 48


def text_windows(path):
    """Up to 8 relocation-free WINDOW-byte runs from the object's .text."""
    data = open(path, "rb").read()
    nsec = struct.unpack_from("<H", data, 2)[0]
    opt = struct.unpack_from("<H", data, 16)[0]
    out = []
    for i in range(nsec):
        off = 20 + opt + 40 * i
        name = data[off:off + 8].rstrip(b"\0")
        size, raw, rel = struct.unpack_from("<III", data, off + 16)
        nrel = struct.unpack_from("<H", data, off + 32)[0]
        if not name.startswith(b".text") or size == 0:
            continue
        text = data[raw:raw + size]
        relocated = set()
        for r in range(nrel):
            va = struct.unpack_from("<I", data, rel + 10 * r)[0]
            relocated.update(range(va, va + 8))
        pos = 0
        while pos + WINDOW <= len(text) and len(out) < 8:
            if not relocated.intersection(range(pos, pos + WINDOW)) and len(set(text[pos:pos + WINDOW])) > 12:
                out.append(text[pos:pos + WINDOW])
                pos += WINDOW * 8
            else:
                pos += 16
    return out


def main():
    objdir, binaries = sys.argv[1], sys.argv[2:]
    needles = []
    for name in OBJECTS:
        path = os.path.join(objdir, name)
        if os.path.exists(path):
            needles += [(name, w) for w in text_windows(path)]
    print(f"{len(needles)} code windows from {', '.join(OBJECTS)}")
    for binary in binaries:
        blob = open(binary, "rb").read()
        hits = sum(1 for _, w in needles if w in blob)
        print(f"{hits:3d}/{len(needles)} found in {binary}")


if __name__ == "__main__":
    main()
