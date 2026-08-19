#!/usr/bin/env node
// Keeps Linux release binaries runnable on the oldest glibc we support.
//
// A binary linked on a newer distro records, per shared library, the set of
// symbol *versions* it needs (`.gnu.version_r`). ld.so refuses to start the
// program when any of those versions is missing on the target machine —
// "version GLIBC_2.39 not found" — and it does so before a single symbol is
// looked up, so it happens even when the only symbols carrying that version
// are *weak* references the program is perfectly happy to find missing.
// Rust's std does exactly that: it weak-references glibc 2.39's
// `pidfd_spawnp`/`pidfd_getpid` and falls back to fork/exec when they are
// absent, which is why a client built on Debian 13 (glibc 2.41) dies on an
// Ubuntu 22.04 box that would otherwise run it fine.
//
// So this drops the requirement: the weak symbols are made unversioned
// (their `.gnu.version` index becomes VER_NDX_GLOBAL) and the now-unreferenced
// entry is unlinked from the library's version-requirement chain — the same
// edit `patchelf --clear-symbol-version` performs, done here so the build
// needs no patchelf on the host. A machine that has the symbols still binds
// them (unversioned lookup finds the default version); one that doesn't
// resolves them to NULL and std takes its fallback path, with no loader
// diagnostic either way. Marking the requirement VER_FLG_WEAK instead would
// also boot, but ld.so prints "weak version ... not found" to stderr on every
// launch on the older machine, which is noise a released client should not
// produce.
//
// Requirements above the floor that are reachable from a *non-weak* symbol
// cannot be waved away like that — the program really would crash — so those
// fail the build instead, loudly, rather than shipping a package that only
// starts on the machine that built it.
//
// Runs from tauri.conf.json's `beforeBundleCommand`, i.e. after cargo has
// linked the binaries and before the bundler copies them into the .deb/.rpm,
// which is the only point where patching them still ends up in the package.
// A no-op on Windows/macOS builds (nothing there parses as a Linux ELF).

import { readFileSync, writeFileSync, existsSync, readdirSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Oldest glibc a released Lumepeer client is expected to start on: Ubuntu
// 22.04 (2.35). Debian 12 (2.36) and RHEL 9 (2.34) clear it too. Matches the
// ubuntu-22.04 runner release.yml builds linux-amd64 on, so CI itself never
// produces anything above this line.
const DEFAULT_FLOOR = "2.35";

const STB_WEAK = 2;
const VER_NDX_GLOBAL = 1;
const VERSYM_MASK = 0x7fff;
const SHDR_SIZE = 64;
const SYM_SIZE = 24;

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

function parseFloor(text) {
  const m = /^(\d+)\.(\d+)$/.exec(text);
  if (!m) throw new Error(`bad glibc floor "${text}", expected e.g. 2.35`);
  return [Number(m[1]), Number(m[2])];
}

function glibcVersion(name) {
  const m = /^GLIBC_(\d+)\.(\d+)(?:\.(\d+))?$/.exec(name);
  return m ? [Number(m[1]), Number(m[2])] : null;
}

function above([major, minor], [floorMajor, floorMinor]) {
  return major > floorMajor || (major === floorMajor && minor > floorMinor);
}

function cstr(buf, off) {
  const end = buf.indexOf(0, off);
  return buf.toString("utf8", off, end < 0 ? buf.length : end);
}

// Returns the ELF64 little-endian section table, or null for anything else
// (PE/Mach-O/scripts on the other release targets, or a 32-bit ELF we build
// no target for).
function sectionTable(buf) {
  if (buf.length < 0x40) return null;
  if (buf[0] !== 0x7f || buf[1] !== 0x45 || buf[2] !== 0x4c || buf[3] !== 0x46) return null;
  if (buf[4] !== 2 || buf[5] !== 1) return null;

  const shoff = Number(buf.readBigUInt64LE(0x28));
  const shentsize = buf.readUInt16LE(0x3a);
  const shnum = buf.readUInt16LE(0x3c);
  const shstrndx = buf.readUInt16LE(0x3e);
  if (shentsize !== SHDR_SIZE || shnum === 0) return null;
  if (shoff + shnum * SHDR_SIZE > buf.length) return null;

  const secs = [];
  for (let i = 0; i < shnum; i++) {
    const at = shoff + i * SHDR_SIZE;
    secs.push({
      nameOff: buf.readUInt32LE(at),
      offset: Number(buf.readBigUInt64LE(at + 24)),
      size: Number(buf.readBigUInt64LE(at + 32)),
      link: buf.readUInt32LE(at + 40),
      info: buf.readUInt32LE(at + 44),
    });
  }
  if (shstrndx >= secs.length) return null;
  const strtab = secs[shstrndx].offset;
  for (const s of secs) s.name = cstr(buf, strtab + s.nameOff);
  return secs;
}

// Every dynamic symbol bound to a given version index, with its binding, so
// the caller can tell "weak reference we can let go missing" from "the
// program actually calls this".
function symbolsForVersion(buf, secs, versionIndex) {
  const versym = secs.find((s) => s.name === ".gnu.version");
  const dynsym = secs.find((s) => s.name === ".dynsym");
  if (!versym || !dynsym) return [];
  const dynstr = secs[dynsym.link];
  if (!dynstr) return [];

  const count = Math.min(Math.floor(versym.size / 2), Math.floor(dynsym.size / SYM_SIZE));
  const found = [];
  for (let i = 0; i < count; i++) {
    if ((buf.readUInt16LE(versym.offset + i * 2) & VERSYM_MASK) !== versionIndex) continue;
    const at = dynsym.offset + i * SYM_SIZE;
    found.push({
      versymOffset: versym.offset + i * 2,
      name: cstr(buf, dynstr.offset + buf.readUInt32LE(at)),
      weak: buf.readUInt8(at + 4) >> 4 === STB_WEAK,
    });
  }
  return found;
}

// The .gnu.version_r chain, read into plain objects: one entry per needed
// library, each holding the versions ("aux" records) wanted from it. Both
// levels are singly linked by byte offset rather than packed as arrays,
// which is what makes dropping one a matter of relinking in place.
function versionNeeds(buf, secs, verneed, dynstr) {
  const entries = [];
  let vnOff = verneed.offset;
  for (let n = 0; n < verneed.info; n++) {
    const cnt = buf.readUInt16LE(vnOff + 2);
    const next = buf.readUInt32LE(vnOff + 12);
    const entry = {
      offset: vnOff,
      file: cstr(buf, dynstr.offset + buf.readUInt32LE(vnOff + 4)),
      auxes: [],
    };

    let auxOff = vnOff + buf.readUInt32LE(vnOff + 8);
    for (let a = 0; a < cnt; a++) {
      const auxNext = buf.readUInt32LE(auxOff + 12);
      entry.auxes.push({
        offset: auxOff,
        other: buf.readUInt16LE(auxOff + 6),
        name: cstr(buf, dynstr.offset + buf.readUInt32LE(auxOff + 8)),
      });
      if (auxNext === 0) break;
      auxOff += auxNext;
    }

    entries.push(entry);
    if (next === 0) break;
    vnOff += next;
  }
  return entries;
}

function patchFile(path, floor, checkOnly) {
  const buf = readFileSync(path);
  const secs = sectionTable(buf);
  if (!secs) return null;

  const verneed = secs.find((s) => s.name === ".gnu.version_r");
  if (!verneed) return null;
  const dynstr = secs[verneed.link];
  if (!dynstr) return null;

  const cleared = [];
  const blocking = [];
  let dirty = false;

  for (const entry of versionNeeds(buf, secs, verneed, dynstr)) {
    const drop = new Set();
    for (const aux of entry.auxes) {
      const version = glibcVersion(aux.name);
      if (!version || !above(version, floor)) continue;

      const syms = symbolsForVersion(buf, secs, aux.other);
      const strong = syms.filter((s) => !s.weak);
      if (strong.length > 0) {
        blocking.push({ file: entry.file, name: aux.name, symbols: strong.map((s) => s.name) });
      } else {
        drop.add(aux);
        aux.symbols = syms;
      }
    }
    if (drop.size === 0) continue;

    const survivors = entry.auxes.filter((aux) => !drop.has(aux));
    if (survivors.length === 0) {
      // Unlinking the last version wanted from a library means unlinking the
      // whole entry, which also means fixing up .dynamic's DT_VERNEEDNUM and
      // possibly moving the chain head. Nothing we build gets there — libc
      // always needs versions from well below the floor — so refuse rather
      // than carry untested ELF surgery.
      blocking.push({
        file: entry.file,
        name: [...drop].map((aux) => aux.name).join(", "),
        symbols: ["<entire version requirement on this library>"],
      });
      continue;
    }

    for (const aux of drop) {
      for (const sym of aux.symbols) buf.writeUInt16LE(VER_NDX_GLOBAL, sym.versymOffset);
      cleared.push({ file: entry.file, name: aux.name, symbols: aux.symbols.map((s) => s.name) });
    }

    buf.writeUInt16LE(survivors.length, entry.offset + 2);
    buf.writeUInt32LE(survivors[0].offset - entry.offset, entry.offset + 8);
    survivors.forEach((aux, i) => {
      const next = i + 1 < survivors.length ? survivors[i + 1].offset - aux.offset : 0;
      buf.writeUInt32LE(next, aux.offset + 12);
    });
    dirty = true;
  }

  if (dirty && !checkOnly) writeFileSync(path, buf);
  return { cleared, blocking };
}

function candidates() {
  const found = [];
  const push = (p) => {
    if (existsSync(p) && statSync(p).isFile()) found.push(p);
  };

  // Every per-target release dir plus the plain host one: the triple in play
  // is not knowable here (`tauri build --target ...` is what picks it), and
  // patching a binary this run does not bundle is a harmless no-op.
  const targetDir = join(ROOT, "target");
  if (existsSync(targetDir)) {
    push(join(targetDir, "release", "lumepeer-desktop"));
    for (const entry of readdirSync(targetDir, { withFileTypes: true })) {
      if (entry.isDirectory()) push(join(targetDir, entry.name, "release", "lumepeer-desktop"));
    }
  }

  // The decoder-worker sidecars staged for `bundle.externalBin`; they are
  // copied into the package verbatim, so they need the same treatment.
  const binDir = join(ROOT, "apps", "desktop", "src-tauri", "binaries");
  if (existsSync(binDir)) {
    for (const entry of readdirSync(binDir, { withFileTypes: true })) {
      if (entry.isFile()) push(join(binDir, entry.name));
    }
  }

  return found;
}

function parseArgs(argv) {
  const out = {
    floor: process.env.LUMEPEER_GLIBC_FLOOR || DEFAULT_FLOOR,
    checkOnly: false,
    paths: [],
  };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--floor") out.floor = argv[++i];
    else if (argv[i] === "--check") out.checkOnly = true;
    else out.paths.push(argv[i]);
  }
  return out;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  const floor = parseFloor(args.floor);
  const paths = args.paths.length > 0 ? args.paths : candidates();

  let blocked = false;
  let pending = 0;
  for (const path of paths) {
    const result = patchFile(path, floor, args.checkOnly);
    if (!result) continue;
    for (const c of result.cleared) {
      pending++;
      const verb = args.checkOnly ? "would drop" : "dropped";
      console.log(
        `glibc-floor: ${path}: ${verb} requirement ${c.name} (${c.file}); weak-only: ${c.symbols.join(", ")}`,
      );
    }
    for (const b of result.blocking) {
      blocked = true;
      console.error(
        `glibc-floor: ${path}: needs ${b.name} (${b.file}) for non-weak symbol(s) ${b.symbols.join(", ")}`,
      );
    }
  }

  if (blocked) {
    console.error(
      `glibc-floor: this build host's glibc exceeds the supported floor ${args.floor} in a way that`,
    );
    console.error(
      "glibc-floor: cannot be patched away. Build this target on an older distro (release.yml uses",
    );
    console.error(
      "glibc-floor: ubuntu-22.04), or raise the floor deliberately via --floor/LUMEPEER_GLIBC_FLOOR",
    );
    console.error("glibc-floor: and record why in docs/adr/.");
    process.exit(1);
  }
  if (args.checkOnly && pending > 0) {
    process.exit(1);
  }
}

main();
