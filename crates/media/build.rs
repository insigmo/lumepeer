//! Stops a Windows release build that would link an unoptimised libopus
//! (ADR 0149).
//!
//! `audio-opus` pulls in `opus`, which builds the vendored libopus through
//! `opusic-sys` and the `cmake` crate. With MSVC and Visual Studio's generator
//! the `cmake` crate writes `CMAKE_C_FLAGS_RELEASE` itself, without the
//! `/O2 /Ob2 /DNDEBUG` it would hold (ADR 0146): libopus compiles at `/Od`
//! and takes three times as long to encode and decode. `opusic-sys` switches to
//! the Ninja generator whenever `ninja --version` runs, and then nothing is
//! overridden. The release runners have ninja, so the libopus that ships is
//! optimised; a machine without it builds a slower one and nothing says so.
//!
//! So this script asks the question `opusic-sys` asks, in the same
//! environment, and a release build stops when the answer is no. A debug
//! build only warns: its own code is unoptimised anyway, and nothing measured
//! or shipped is built that way.
//!
//! `opusic-sys` does not rebuild libopus when `PATH` changes, so installing
//! ninja is not enough for a target directory that already holds a libopus;
//! the message says how to rebuild it.

use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PATH");

    // The variables the `cmake` crate reads a generator from. A generator
    // named in any of them is used as is, and the flags are only overridden
    // when none is.
    let target = env::var("TARGET").unwrap_or_default();
    let kind = if env::var("HOST").ok().as_deref() == Some(target.as_str()) {
        "HOST"
    } else {
        "TARGET"
    };
    let generator_vars = [
        format!("CMAKE_GENERATOR_{target}"),
        format!("CMAKE_GENERATOR_{}", target.replace('-', "_")),
        format!("{kind}_CMAKE_GENERATOR"),
        "CMAKE_GENERATOR".to_owned(),
    ];
    for var in &generator_vars {
        println!("cargo:rerun-if-env-changed={var}");
    }

    if env::var_os("CARGO_FEATURE_AUDIO_OPUS").is_none()
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
        || generator_vars.iter().any(|var| env::var_os(var).is_some())
    {
        return;
    }
    let has_ninja = Command::new("ninja")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if has_ninja {
        return;
    }

    let release = env::var("PROFILE").as_deref() == Ok("release");
    let message = format!(
        "ninja is not on PATH, so opusic-sys builds libopus with Visual Studio's generator, \
         where the cmake crate drops its /O2: libopus would be compiled unoptimised, three times \
         slower than the one that ships (ADR 0149). Install ninja (`winget install \
         Ninja-build.Ninja` or `choco install ninja`), put it on PATH, then run `cargo clean -p \
         opusic-sys{}` (with this build's --target, if it has one) so libopus is built again.",
        if release { " --release" } else { "" },
    );
    assert!(!release, "{message}");
    println!("cargo:warning={message}");
}
