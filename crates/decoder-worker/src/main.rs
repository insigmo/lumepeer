//! Sandboxed decoder worker (design doc §4, §11.3).
//!
//! Runs as its own OS process with a platform sandbox and no network or
//! filesystem access beyond the descriptors handed to it. Frames go back to the
//! main process over a shared memory ring buffer, never per-frame
//! serialization (§11.3, §15).
//!
//! It refuses to decode when the sandbox cannot be applied: better no video
//! than an unconfined decoder in the trust boundary.

#![forbid(unsafe_code)]

use lumepeer_media::decode::platform_sandbox;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let Some(sandbox) = platform_sandbox() else {
        anyhow::bail!("no sandbox mechanism available on this platform; refusing to decode");
    };
    tracing::info!(?sandbox, "decoder worker starting");

    anyhow::bail!("phase 2: apply sandbox, attach ring buffer, run decode loop (§11.3)")
}
