#!/usr/bin/env python3
"""Does a binary carry libaom's x86 assembly? (ADR 0141)

`lumepeer-aom-sys` refuses to build without nasm, and checks that libaom
configured itself with its x86 kernels. This is the check on the other end:
that the machine code nasm produced actually reached the binary that ships.
It is the libaom counterpart of tools/codec-bench/find-openh264-asm.py
(research/software-av1), which found the openh264 fallback can silently ship
without its assembly.

It reads the objects nasm assembled from libaom's `.asm` sources in the
`lumepeer-aom-sys` build tree — ELF on Linux, COFF on Windows — takes runs of
their `.text` that no relocation touches, and looks for each run in the
binary. Code a linker copied in unchanged is found byte for byte; a build
that never assembled anything has nothing to find.

  python ci/find-libaom-asm.py <libaom build dir> <binary> [<binary> ...]

The build dir is `<target dir>/<profile>/build/lumepeer-aom-sys-*/out/build`
(a glob is accepted). Exits non-zero unless every object in REQUIRED was
assembled and every run of its code is in every binary. The other objects
are counted and reported, not required: a realtime-only libaom never calls
some of them (the SSIM tuning, the transforms its C intrinsics replace), and
a linker is right to leave out an object nothing calls.
"""
import glob
import os
import struct
import sys

WINDOW = 48
PER_OBJECT = 6
# The assembly every realtime encode runs through: sums of absolute
# differences, sub-pixel variance, intra prediction, quantisation, the forward
# transform and the residual. At least these must have been assembled.
REQUIRED = [
    "sad_sse2",
    "sad4d_sse2",
    "subpel_variance_ssse3",
    "intrapred_asm_sse2",
    "quantize_ssse3_x86_64",
    "subtract_sse2",
]


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
    if data[4] != 2 or data[5] != 1:
        raise ValueError("not a little-endian ELF64 object")
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
        elif stype == 9:  # SHT_REL
            for r in range(size // 16):
                at = struct.unpack_from("<Q", data, offset + 16 * r)[0]
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


# The `.asm` sources of the vendored libaom. None shares its name with a C
# source, so an object named after one of them can only be nasm's.
SOURCE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "crates", "aom-sys", "libaom")
ASM_STEMS = {
    os.path.basename(path)[: -len(".asm")]
    for path in glob.glob(os.path.join(SOURCE, "**", "*.asm"), recursive=True)
}


def asm_stem(path):
    """The `.asm` source an object was assembled from, or None.

    Makefile and Ninja generators name the object `<stem>.asm.o(bj)`; Visual
    Studio's names it `<stem>.obj`.
    """
    name = os.path.basename(path)
    for suffix in (".asm.o", ".asm.obj", ".obj"):
        if name.endswith(suffix):
            stem = name[: -len(suffix)]
            return stem if stem in ASM_STEMS else None
    return None


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    roots = glob.glob(sys.argv[1]) or [sys.argv[1]]
    objects = {}
    for root in roots:
        for path in glob.glob(os.path.join(root, "**", "*"), recursive=True):
            stem = asm_stem(path)
            if stem and os.path.isfile(path):
                objects.setdefault(stem, path)
    missing = [stem for stem in REQUIRED if stem not in objects]
    if not objects or missing:
        print(f"no nasm-assembled libaom objects for {missing or 'anything'} under {roots}")
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
        absent = sorted(stem for stem, (h, n) in hits.items() if n and h == 0)
        if absent:
            print(f"     absent: {', '.join(absent)}")
        short = [stem for stem in REQUIRED if hits.get(stem, [0, 1])[0] < hits.get(stem, [0, 1])[1]]
        if short:
            print(f"     FAIL: libaom's assembly for {', '.join(short)} is not in {binary}")
            failed = True
        else:
            print(f"     ok: all of {', '.join(REQUIRED)}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
