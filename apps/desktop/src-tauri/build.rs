/// Every IPC command of §13, declared so Tauri autogenerates an `allow-<name>`
/// permission for each one.
///
/// Declaring an app manifest at all is what makes Tauri apply its ACL to this
/// application's own commands, not just to plugin commands: from here on a
/// command is callable only from a window whose capability names it. That is
/// the deny-by-default rule of §2.1 applied to the IPC boundary, and it is what
/// lets `capabilities/view.json` hand a remote-view window exactly four
/// commands and nothing else.
const COMMANDS: &[&str] = &[
    "session_grant",
    "session_revoke",
    "session_status",
    "license_status",
    "invite_create",
    "invite_connect",
    "view_next_frame",
    "input_pointer_move",
    "input_press",
    "input_wheel",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .unwrap_or_else(|error| panic!("failed to run tauri-build: {error}"));
}
