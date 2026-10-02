#!/usr/bin/env python3
"""Packs IVF streams into decode-data.js for decode-bench.html (file:// cannot fetch).

  python make-decode-data.py out/decode-data.js name=path.ivf[:frames] [...]

An optional :frames keeps only the first that many pictures of a stream.
"""
import base64
import json
import struct
import sys


def trimmed(path, limit):
    with open(path, "rb") as f:
        data = f.read()
    if not limit:
        return data
    off, n = 32, 0
    while off + 12 <= len(data) and n < limit:
        off += 12 + struct.unpack_from("<I", data, off)[0]
        n += 1
    header = bytearray(data[:32])
    header[24:28] = struct.pack("<I", n)
    return bytes(header) + data[32:off]


def main():
    out, pairs = sys.argv[1], sys.argv[2:]
    streams = {}
    for pair in pairs:
        name, spec = pair.split("=", 1)
        path, _, limit = spec.rpartition(":") if spec.rsplit(":", 1)[-1].isdigit() else (spec, "", "")
        streams[name] = base64.b64encode(trimmed(path, int(limit) if limit else 0)).decode()
    with open(out, "w") as f:
        f.write("window.DECODE_DATA = " + json.dumps(streams) + ";\n")


if __name__ == "__main__":
    main()
