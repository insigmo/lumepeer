#!/usr/bin/env python3
"""Were the CMake-built C libraries compiled optimised? (ADR 0146, ADR 0149)

The `cmake` crate, with MSVC and Visual Studio's generator, writes
`CMAKE_<LANG>_FLAGS_<CONFIG>` itself and leaves CMake's optimisation out.
`lumepeer-aom-sys` puts it back and checks at build time (ADR 0146).
`opusic-sys` avoids the override only by building with Ninja when `ninja` is
on PATH, and `lumepeer-media`'s build script stops a release build without
it (ADR 0149). Neither build script can see what another crate's build
produced, and a cached target directory can hold a libopus built before
either rule existed: `opusic-sys` does not rebuild when PATH changes. This
reads what the build actually left on disk.

For every build directory, it reads `CMakeCache.txt`, takes the build type
the cache was configured for, and requires that configuration's C flags to
hold an optimisation switch and `NDEBUG`.

  python ci/check-cmake-optimised.py <build dir> [<build dir> ...]

A build dir is `<target dir>/<profile>/build/<crate>-*/out/build`; a glob is
accepted and every glob must match at least one build. Exits non-zero
otherwise, or when any of them is not optimised.
"""
import glob
import os
import sys

OPTIMISE = {"/O1", "/O2", "/Ox", "-O1", "-O2", "-O3", "-Os", "-Oz"}
NDEBUG = {"/DNDEBUG", "-DNDEBUG"}


def cache_value(text, name):
    """The value of `name:TYPE=value` in a CMakeCache.txt, or None."""
    for line in text.splitlines():
        key, sep, value = line.partition("=")
        if sep and key.split(":", 1)[0] == name:
            return value.strip()
    return None


def check(build_dir):
    path = os.path.join(build_dir, "CMakeCache.txt")
    with open(path, encoding="utf-8", errors="replace") as cache:
        text = cache.read()
    build_type = cache_value(text, "CMAKE_BUILD_TYPE") or ""
    var = f"CMAKE_C_FLAGS_{build_type.upper()}"
    flags = (cache_value(text, var) or "").split()
    generator = cache_value(text, "CMAKE_GENERATOR")
    good = bool(OPTIMISE.intersection(flags)) and bool(NDEBUG.intersection(flags))
    verdict = "ok" if good else "NOT OPTIMISED"
    print(f"{verdict}: {build_dir}\n  {generator}, {var} = {' '.join(flags) or '(empty)'}")
    return good


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    failed = False
    for pattern in sys.argv[1:]:
        dirs = sorted(d for d in glob.glob(pattern) if os.path.isfile(os.path.join(d, "CMakeCache.txt")))
        if not dirs:
            print(f"NO BUILD: nothing matches {pattern}")
            failed = True
        for build_dir in dirs:
            if not check(build_dir):
                failed = True
    sys.exit(1 if failed else 0)


main()
