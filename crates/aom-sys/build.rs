//! Builds the vendored libaom (`libaom/`, release 3.15.1) as a static library
//! with its x86 assembly, and the C shim over it (ADR 0139).
//!
//! The assembly is not optional. `openh264-sys2` silently builds without its
//! SIMD when it cannot find `nasm`, and the measurement that found it timed
//! that build at four to eight times slower than the real one
//! (docs/research/software-av1.md). A software encoder that is only fit to
//! ship because it is fast must not be able to come out slow without anyone
//! noticing, so a build with no usable `nasm` stops here with the reason,
//! and a build whose configuration did not come out with AVX2 and runtime CPU
//! detection stops right after libaom's own configure step.
//!
//! `NASM` may name the assembler to use; otherwise `nasm` is looked up on
//! `PATH`.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The oldest nasm libaom's configure step accepts (`cmake/aom_optimization.cmake`).
const NASM_MIN: (u32, u32) = (2, 14);

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=shim/aom_shim.c");
    // A directory: cargo watches every file under it.
    println!("cargo:rerun-if-changed=libaom");
    println!("cargo:rerun-if-env-changed=NASM");

    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    assert!(
        arch == "x86_64" && (os == "windows" || os == "linux"),
        "lumepeer-aom-sys is built for x86-64 Windows and Linux only (ADR 0139): \
         software AV1 was measured nowhere else. This target is {arch}-{os}; \
         `encode-aom` should not have pulled this crate in."
    );

    let nasm = find_nasm();
    let nasm_dir = nasm.parent().map_or_else(PathBuf::new, Path::to_path_buf);

    let mut aom = cmake::Config::new("libaom");
    // Always optimised, whatever the cargo profile: a debug libaom is far
    // too slow to test a realtime encoder against.
    aom.profile("Release")
        .define("AOM_TARGET_CPU", "x86_64")
        .define("ENABLE_NASM", "1")
        .define("CONFIG_RUNTIME_CPU_DETECT", "1")
        .define("CONFIG_REALTIME_ONLY", "1")
        .define("CONFIG_AV1_DECODER", "0")
        .define("CONFIG_LIBYUV", "0")
        .define("CONFIG_WEBM_IO", "0")
        .define("ENABLE_APPS", "0")
        .define("ENABLE_DOCS", "0")
        .define("ENABLE_EXAMPLES", "0")
        .define("ENABLE_TESTDATA", "0")
        .define("ENABLE_TESTS", "0")
        .define("ENABLE_TOOLS", "0")
        .define("CMAKE_INSTALL_LIBDIR", "lib")
        // libaom's configure step drops any assembler it was handed when
        // ENABLE_NASM is on and runs its own `find_program(nasm $ENV{NASM_PATH})`,
        // so the one found above is passed the way it reads it.
        .env("NASM_PATH", &nasm_dir);
    if os == "linux" {
        // Rust links position-independent executables; the assembly has to
        // be assembled for that too (`-DPIC`), not only the C.
        aom.define("CONFIG_PIC", "1");
    }
    let dst = aom.build();

    check_configuration(&dst.join("build").join("config").join("aom_config.h"));

    let include = dst.join("include");
    // Compiled before libaom is named, so a single-pass linker sees the shim
    // ask for libaom's symbols before it reaches the library that has them.
    cc::Build::new()
        .file("shim/aom_shim.c")
        .include(&include)
        .opt_level(2)
        .compile("aom_shim");
    println!(
        "cargo:rustc-link-search=native={}",
        dst.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=aom");
    if os == "linux" {
        println!("cargo:rustc-link-lib=dylib=m");
        println!("cargo:rustc-link-lib=dylib=pthread");
    }
    // DEP_AOM_INCLUDE / DEP_AOM_BUILD for a dependent that wants the headers,
    // or the assembled objects (ci/find-libaom-asm.py).
    println!("cargo:include={}", include.display());
    println!("cargo:build={}", dst.join("build").display());
}

/// The `nasm` to assemble libaom with, or a build failure saying how to get
/// one.
fn find_nasm() -> PathBuf {
    let candidate = env::var_os("NASM").map_or_else(
        || which("nasm").unwrap_or_else(|| PathBuf::from("nasm")),
        PathBuf::from,
    );
    let output = Command::new(&candidate).arg("-v").output();
    let version = match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => panic!(
            "nasm not found (tried {}). libaom's x86 assembly is required, not optional \
             (ADR 0139): without it the encoder is several times slower and nothing would \
             say so. Install nasm {}.{} or newer — `apt install nasm`, `dnf install nasm`, \
             `choco install nasm` or `winget install NASM.NASM` — and put it on PATH, or \
             set NASM to its full path.",
            candidate.display(),
            NASM_MIN.0,
            NASM_MIN.1,
        ),
    };
    let found = parse_nasm_version(&version);
    assert!(
        found.is_some_and(|found| found >= NASM_MIN),
        "{} reports {:?}; libaom needs nasm {}.{} or newer (ADR 0139)",
        candidate.display(),
        version.trim(),
        NASM_MIN.0,
        NASM_MIN.1,
    );
    candidate
}

/// `(major, minor)` out of `NASM version 2.16.01 compiled on ...`.
fn parse_nasm_version(banner: &str) -> Option<(u32, u32)> {
    let rest = banner.split("version").nth(1)?.trim_start();
    let number: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = number.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The first `name` on `PATH`, with the platform's executable suffix.
fn which(name: &str) -> Option<PathBuf> {
    let file = format!("{name}{}", env::consts::EXE_SUFFIX);
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(&file))
        .find(|path| path.is_file())
}

/// Stops the build unless libaom configured itself for what was measured:
/// x86-64, AVX2 kernels compiled in and chosen at run time.
fn check_configuration(header: &Path) {
    let config = std::fs::read_to_string(header)
        .unwrap_or_else(|error| panic!("cannot read libaom's {}: {error}", header.display()));
    for wanted in [
        "AOM_ARCH_X86_64 1",
        "HAVE_SSE2 1",
        "HAVE_SSSE3 1",
        "HAVE_SSE4_1 1",
        "HAVE_AVX2 1",
        "CONFIG_RUNTIME_CPU_DETECT 1",
        "CONFIG_REALTIME_ONLY 1",
    ] {
        assert!(
            config.contains(&format!("#define {wanted}")),
            "libaom configured without `{wanted}` ({}); the build would not be the one \
             ADR 0139 measured",
            header.display()
        );
    }
}
