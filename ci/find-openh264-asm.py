#!/usr/bin/env python3
"""Does a binary carry openh264's x86 assembly? (ADR 0157)

openh264-sys2 assembles openh264's SIMD with nasm when nasm runs and quietly
builds C alone when it does not, and nothing tells cargo to try again once
nasm is there: a cached build made without it is reused for good. Every Linux
release from 2026-10-02 to v0.0.145 shipped that way, and the openh264
fallback took 70-110 ms a 1280x800 frame on a host without a hardware encoder.

This reads the objects nasm assembled in the openh264-sys2 build tree (ELF on
Linux, COFF on Windows), takes runs of their code that no relocation touches,
and looks for each run in the binary — the method of ci/find-libaom-asm.py.

  python ci/find-openh264-asm.py <openh264-sys2 out dir> <binary> [<binary> ...]

The out dir is `<target dir>/<profile>/build/openh264-sys2-*/out` (a glob is
accepted). Exits non-zero unless every object in REQUIRED was assembled and
every run of its code is in every binary.
"""
import glob
import os
import struct
import sys

WINDOW = 48
PER_OBJECT = 6
# What every encoded frame runs through: sums of differences, luma motion
# compensation, quantisation, the transform, and the screen-content analysis.
REQUIRED = ["satd_sad", "mc_luma", "quant", "dct", "vaa"]


def code_windows(text, relocated):
    """Up to PER_OBJECT relocation-free WINDOW-byte runs of `text`."""
    out = []
    pos = 0
    while pos + WINDOW <= len(text) and len(out) < PER_OBJECT:
        run = text[pos:pos + WINDOW]
        if not relocated.intersection(range(pos, pos + WINDOW)) and len(set(run)) > 12:
            out.append(run)
            pos += WINDOW * 4
        else:
            pos += 16
    return out


def elf_windows(data):
    """Windows from every executable section of an ELF64 relocatable object."""
    shoff = struct.unpack_from("<Q", data, 0x28)[0]
    shentsize, shnum = struct.unpack_from("<HH", data, 0x3A)
    sections = []
    for i in range(shnum):
        off = shoff + i * shentsize
        _name, stype, flags, _addr, offset, size, _link, info = struct.unpack_from(
            "<IIQQQQII", data, off
        )
        sections.append((stype, flags, offset, size, info))
    relocated = {}
    for stype, _flags, offset, size, info in sections:
        if stype == 4:  # SHT_RELA
            for r in range(size // 24):
                at = struct.unpack_from("<Q", data, offset + 24 * r)[0]
                relocated.setdefault(info, set()).update(range(at, at + 8))
    out = []
    for index, (stype, flags, offset, size, _info) in enumerate(sections):
        if stype == 1 and flags & 0x4 and size:  # SHT_PROGBITS, SHF_EXECINSTR
            out += code_windows(data[offset:offset + size], relocated.get(index, set()))
    return out


def coff_windows(data):
    """Windows from every `.text` section of a COFF object."""
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
        relocated = set()
        for r in range(nrel):
            va = struct.unpack_from("<I", data, rel + 10 * r)[0]
            relocated.update(range(va, va + 8))
        out += code_windows(data[raw:raw + size], relocated)
    return out


def object_windows(path):
    data = open(path, "rb").read()
    if data[:4] == b"\x7fELF":
        return elf_windows(data)
    return coff_windows(data)


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    roots = glob.glob(sys.argv[1]) or [sys.argv[1]]
    objects = {}
    for root in roots:
        for path in glob.glob(os.path.join(root, "**", "*"), recursive=True):
            name = os.path.basename(path)
            # nasm-rs names an object after its source alone; cc prefixes
            # every C object with a hash, so these names are nasm's.
            for stem in REQUIRED:
                if name in (f"{stem}.o", f"{stem}.obj") and os.path.isfile(path):
                    objects.setdefault(stem, path)
    missing = [stem for stem in REQUIRED if stem not in objects]
    if missing:
        print(f"no nasm-assembled openh264 objects for {missing} under {roots}")
        return 1
    needles = []
    for stem, path in sorted(objects.items()):
        needles += [(stem, w) for w in object_windows(path)]
    print(f"{len(needles)} code windows from {len(objects)} nasm objects")
    failed = False
    for binary in sys.argv[2:]:
        blob = open(binary, "rb").read()
        hits = {}
        for stem, window in needles:
            hits.setdefault(stem, [0, 0])
            hits[stem][1] += 1
            if window in blob:
                hits[stem][0] += 1
        found = sum(h for h, _ in hits.values())
        print(f"{found:4d}/{len(needles)} code windows found in {binary}")
        short = [stem for stem, (h, n) in sorted(hits.items()) if h < n]
        if short:
            print(f"     FAIL: openh264's assembly for {', '.join(short)} is not in {binary}")
            failed = True
        else:
            print(f"     ok: all of {', '.join(REQUIRED)}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
