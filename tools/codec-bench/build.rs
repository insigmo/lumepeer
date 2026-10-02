//! Compiles the C shims over libaom and SVT-AV1 and links the static libraries
//! built from their sources (tools/codec-bench/build-libs.sh).
//!
//! AOM_INCLUDE / AOM_LIB_DIR: libaom's source root (holds `aom/aom_encoder.h`)
//! and the directory with `aom.lib` / `libaom.a`.
//! SVT_INCLUDE / SVT_LIB_DIR: SVT-AV1's `Source/API` and the directory with
//! `SvtAv1Enc.lib` / `libSvtAv1Enc.a`.

use std::env;

fn var(name: &str) -> String {
    println!("cargo:rerun-if-env-changed={name}");
    env::var(name).unwrap_or_else(|_| panic!("{name} is not set; see build.rs"))
}

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    // openh264-sys2 silently builds without its x86 assembly when it cannot
    // run nasm, or when OPENH264_NO_ASM is set; record which case this binary
    // was built under, because it changes the openh264 numbers several-fold.
    println!("cargo:rerun-if-env-changed=OPENH264_NO_ASM");
    println!("cargo:rerun-if-env-changed=PATH");
    let nasm = std::process::Command::new("nasm").arg("-v").output().is_ok_and(|o| o.status.success());
    let asm = if env::var_os("OPENH264_NO_ASM").is_some() {
        "no (OPENH264_NO_ASM)"
    } else if nasm {
        "yes"
    } else {
        "no (nasm not found)"
    };
    println!("cargo:rustc-env=CB_OPENH264_ASM={asm}");
    if env::var_os("CARGO_FEATURE_AOM").is_some() {
        println!("cargo:rerun-if-changed=shim/aom_shim.c");
        let include = var("AOM_INCLUDE");
        cc::Build::new()
            .file("shim/aom_shim.c")
            .include(&include)
            .opt_level(2)
            .compile("aom_shim");
        println!("cargo:rustc-link-search=native={}", var("AOM_LIB_DIR"));
        println!("cargo:rustc-link-lib=static=aom");
    }
    if env::var_os("CARGO_FEATURE_SVT").is_some() {
        println!("cargo:rerun-if-changed=shim/svt_shim.c");
        let include = var("SVT_INCLUDE");
        cc::Build::new()
            .file("shim/svt_shim.c")
            .include(&include)
            .opt_level(2)
            .compile("svt_shim");
        println!("cargo:rustc-link-search=native={}", var("SVT_LIB_DIR"));
        println!("cargo:rustc-link-lib=static=SvtAv1Enc");
    }
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=dylib=m");
        println!("cargo:rustc-link-lib=dylib=pthread");
        println!("cargo:rustc-link-lib=dylib=dl");
    }
}
