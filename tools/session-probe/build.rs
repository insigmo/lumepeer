//! Embeds an application manifest. `new_manifest` already carries the
//! Common-Controls v6 dependency, so `TaskDialogIndirect` (measurement 2.5)
//! resolves at load instead of dying with STATUS_ENTRYPOINT_NOT_FOUND. The
//! crate also suppresses rustc's own default manifest, avoiding the mt.exe
//! merge conflict a hand-rolled `/MANIFESTINPUT` hit. Default execution level
//! is asInvoker (the probe's children stay unelevated; `--run` is elevated by
//! Start-Process -Verb RunAs).

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest::embed_manifest(embed_manifest::new_manifest("Lumepeer.SessionProbe"))
            .expect("unable to embed the probe manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
