fn main() {
    // Default build reads capabilities/ (main.json only) via tauri-build's
    // built-in default pattern. The `pilot` feature (debug-only tauri-pilot
    // integration, see Cargo.toml/main.rs) points instead at
    // capabilities-pilot/, which carries both main.json and pilot.json —
    // Tauri validates every capability file's permissions against the
    // plugins actually compiled in, so pilot.json's `pilot:default`
    // permission must never be visible to a build that doesn't compile the
    // plugin, or the build fails.
    if cfg!(feature = "pilot") {
        println!("cargo:rerun-if-changed=capabilities-pilot");
        let attrs =
            tauri_build::Attributes::new().capabilities_path_pattern("./capabilities-pilot/**/*");
        tauri_build::try_build(attrs).unwrap_or_else(|error| {
            panic!("failed to run tauri-build with pilot capabilities: {error}")
        });
    } else {
        tauri_build::build();
    }
}
