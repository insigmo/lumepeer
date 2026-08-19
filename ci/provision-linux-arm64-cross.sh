#!/usr/bin/env bash
# Provisions everything the linux-arm64 client build needs on an amd64 Debian
# host, without touching the system: an aarch64 cross toolchain and an arm64
# sysroot holding the gtk/webkit/appindicator development files tauri links
# against.
#
# Why not `sudo apt-get install libwebkit2gtk-4.1-dev:arm64` and friends:
# libwebkit2gtk-4.1-dev, libayatana-appindicator3-dev and libxdo-dev are
# Multi-Arch: no on Debian, so apt cannot hold the amd64 and arm64 copies at
# once — installing the arm64 ones *removes* the amd64 ones and breaks the
# linux-amd64 build on the same machine. Unpacking the arm64 packages into a
# private sysroot instead leaves the host's own packages alone, and as a
# bonus needs no root at all: apt is pointed at a throwaway state directory
# (`Dir=`) with an empty dpkg status file, so it resolves the dependency
# closure from scratch and only downloads.
#
# Everything lands under $1 (Taskfile.yml passes <repo>/.cross, gitignored)
# and is torn down by deleting that directory. Re-running is a no-op for
# Taskfile.yml, which gates this on the stamp file written at the end.
set -euo pipefail

CROSS_DIR=${1:?usage: provision-linux-arm64-cross.sh <cross-dir>}
TC="$CROSS_DIR/toolchain"
SYSROOT="$CROSS_DIR/sysroot"
BIN="$CROSS_DIR/bin"
WORK="$CROSS_DIR/apt-work"
STAMP="$CROSS_DIR/stamp"

# The sysroot has to come from the same suite as the host so the cross
# compiler (an amd64 package from that same suite) and the arm64 libraries
# agree on the glibc they were built against.
# shellcheck disable=SC1091
. /etc/os-release
SUITE=${VERSION_CODENAME:?/etc/os-release has no VERSION_CODENAME - not a Debian release?}
MIRROR=${LUMEPEER_DEBIAN_MIRROR:-http://deb.debian.org/debian}
KEYRING=/usr/share/keyrings/debian-archive-keyring.gpg

# Mirrors the apt-get line release.yml runs on its Linux runners, minus the
# packages only the host build needs (rpm) - the rest is what tauri's linux
# target links: webkit2gtk, gtk3, the tray indicator, librsvg and libxdo.
SYSROOT_PACKAGES="libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev"
TOOLCHAIN_PACKAGES="gcc-aarch64-linux-gnu g++-aarch64-linux-gnu"

for tool in apt-get dpkg python3; do
  command -v "$tool" >/dev/null || { echo "provision: $tool not found on PATH" >&2; exit 1; }
done
[ -r "$KEYRING" ] || { echo "provision: missing $KEYRING (install debian-archive-keyring)" >&2; exit 1; }

# Downloads the full dependency closure of $3.. for architecture $1 and
# unpacks it into $2. No dpkg database is touched: `dpkg -x` is a plain
# archive extraction, maintainer scripts never run.
fetch_into() {
  local arch=$1 dest=$2
  shift 2
  rm -rf "$WORK"
  mkdir -p "$WORK/etc/apt/apt.conf.d" "$WORK/etc/apt/preferences.d" \
           "$WORK/var/lib/apt/lists/partial" "$WORK/var/lib/dpkg" \
           "$WORK/var/cache/apt/archives/partial"
  : > "$WORK/var/lib/dpkg/status"
  printf 'deb [arch=%s signed-by=%s] %s %s main\n' "$arch" "$KEYRING" "$MIRROR" "$SUITE" \
    > "$WORK/etc/apt/sources.list"

  local apt=(apt-get
    -o "Dir=$WORK"
    -o "Dir::State::status=$WORK/var/lib/dpkg/status"
    -o "APT::Architecture=$arch"
    -o "Dir::Etc::sourcelist=$WORK/etc/apt/sources.list"
    -o "Dir::Etc::sourceparts=-"
    -o "Acquire::Languages=none"
    -qq)
  "${apt[@]}" update
  "${apt[@]}" install --download-only -y --no-install-recommends "$@"

  rm -rf "$dest"
  mkdir -p "$dest"
  local deb
  for deb in "$WORK/var/cache/apt/archives"/*.deb; do
    dpkg -x "$deb" "$dest"
  done
  rm -rf "$WORK"
}

echo "provision: fetching aarch64 cross toolchain ($SUITE)"
fetch_into amd64 "$TC" $TOOLCHAIN_PACKAGES

echo "provision: fetching arm64 sysroot ($SUITE)"
fetch_into arm64 "$SYSROOT" $SYSROOT_PACKAGES

# Debian ships a handful of absolute symlinks (/usr/lib/<triple>/libfoo.so ->
# /lib/<triple>/libfoo.so.N) and, for glibc, ld scripts naming their members
# by absolute path (`GROUP ( /lib/<triple>/libc.so.6 ... )`). Read out of a
# sysroot both escape to the host's amd64 libraries - the symlinks resolve to
# the wrong architecture, and ld reports the script's members as missing
# outright ("cannot find /lib/aarch64-linux-gnu/libm.so.6"), since nothing
# remaps them: --sysroot points at the toolchain, which is where the cross
# libc's own linker scripts have to keep resolving. Rewrite both to stay
# inside the sysroot.
python3 - "$SYSROOT" <<'FIXUP'
import os, re, sys

root = sys.argv[1]

symlinks = 0
for dirpath, dirnames, filenames in os.walk(root):
    for name in dirnames + filenames:
        path = os.path.join(dirpath, name)
        if not os.path.islink(path):
            continue
        target = os.readlink(path)
        if not target.startswith("/"):
            continue
        os.remove(path)
        os.symlink(os.path.relpath(os.path.join(root, target.lstrip("/")), dirpath), path)
        symlinks += 1

# Only a path that starts a token is a path: this must not touch the "/*" that
# opens the script's comment, nor the separators inside a path it just
# rewrote.
ABSOLUTE = re.compile(r"(?<![^\s(=])/(?=[A-Za-z])")
scripts = 0
seen = set()
for libdir in ("usr/lib", "lib"):
    # /lib is a symlink to /usr/lib on a usrmerged Debian, so without this the
    # same script would be rewritten twice and end up with a doubled prefix.
    base = os.path.realpath(os.path.join(root, libdir, "aarch64-linux-gnu"))
    if not os.path.isdir(base) or base in seen:
        continue
    seen.add(base)
    for name in os.listdir(base):
        path = os.path.join(base, name)
        if os.path.islink(path) or not os.path.isfile(path) or os.path.getsize(path) > 8192:
            continue
        with open(path, "rb") as fh:
            raw = fh.read()
        try:
            text = raw.decode("ascii")
        except UnicodeDecodeError:
            continue
        if not re.match(r"\s*(/\*|GROUP|INPUT|OUTPUT_FORMAT)", text) or root in text:
            continue
        patched = ABSOLUTE.sub(root + "/", text)
        if patched != text:
            with open(path, "w") as fh:
                fh.write(patched)
            scripts += 1

print(f"provision: rewrote {symlinks} absolute symlinks and {scripts} ld scripts inside the sysroot")
FIXUP

# Everything the cross compilers need that a bare argv cannot carry lives in
# these wrappers, so Taskfile.yml only has to name them as CC/CXX/linker:
#
#   LD_LIBRARY_PATH - the cross binutils link against their own libopcodes/
#     libbfd, which sit in the unpacked tree rather than the host's /usr/lib,
#     so `as` dies with "libopcodes-*.so: cannot open shared object file"
#     without it.
#   --sysroot       - points at the *toolchain*, so the cross libc's linker
#     scripts resolve; the sysroot's own scripts were rewritten above to hold
#     absolute paths into the sysroot instead.
#   -L/-rpath-link  - ld needs to find the sysroot's transitive shared
#     libraries (libglib pulling in libpcre2 and so on) even though only the
#     direct ones appear on the link line.
mkdir -p "$BIN"
for tool in gcc g++; do
  cat > "$BIN/aarch64-linux-gnu-$tool" <<WRAPPER
#!/bin/sh
export LD_LIBRARY_PATH="$TC/usr/lib/x86_64-linux-gnu:$TC/lib/x86_64-linux-gnu\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "$TC/usr/bin/aarch64-linux-gnu-$tool" \
  --sysroot="$TC" \
  -L"$SYSROOT/usr/lib/aarch64-linux-gnu" \
  -Wl,-rpath-link,"$SYSROOT/usr/lib/aarch64-linux-gnu" \
  "\$@"
WRAPPER
  chmod +x "$BIN/aarch64-linux-gnu-$tool"
done
# ar takes no --sysroot; it needs the library path all the same.
cat > "$BIN/aarch64-linux-gnu-ar" <<WRAPPER
#!/bin/sh
export LD_LIBRARY_PATH="$TC/usr/lib/x86_64-linux-gnu:$TC/lib/x86_64-linux-gnu\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "$TC/usr/bin/aarch64-linux-gnu-ar" "\$@"
WRAPPER
chmod +x "$BIN/aarch64-linux-gnu-ar"

# Smoke-test the whole thing rather than leaving a half-working tree behind a
# stamp file: compile and link a C++ program against the sysroot's glib.
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cat > "$tmp/probe.cpp" <<'PROBE'
#include <glib.h>
#include <string>
int main() { std::string s = g_get_prgname() ? "named" : "anon"; return s.empty(); }
PROBE
PKG_CONFIG_SYSROOT_DIR="$SYSROOT" \
PKG_CONFIG_LIBDIR="$SYSROOT/usr/lib/aarch64-linux-gnu/pkgconfig:$SYSROOT/usr/share/pkgconfig:$SYSROOT/usr/lib/pkgconfig" \
PKG_CONFIG_ALLOW_CROSS=1 \
  bash -c '"$0" "$1" $(pkg-config --cflags --libs glib-2.0) -o "$2"' \
    "$BIN/aarch64-linux-gnu-g++" "$tmp/probe.cpp" "$tmp/probe"
case "$(od -An -tx1 -N20 "$tmp/probe" | tr -d ' \n')" in
  7f454c46020101*00b700*) ;;                       # ELF64 LE, e_machine 0xb7 = AArch64
  *) echo "provision: smoke test produced a non-aarch64 binary" >&2; exit 1 ;;
esac

printf '%s %s\n' "$SUITE" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$STAMP"
echo "provision: ready ($(du -sh "$CROSS_DIR" | cut -f1) under $CROSS_DIR)"
