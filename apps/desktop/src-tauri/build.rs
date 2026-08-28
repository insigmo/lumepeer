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
    "session_set_grant",
    "address_book_list",
    "address_book_upsert",
    "address_book_remove",
    "address_book_set_trusted",
    "unattended_status",
    "unattended_set_password",
    "unattended_disable",
    "unattended_set_totp",
    "unattended_set_role",
    "unattended_submit",
    "connection_history",
    "history_connect",
    "history_remove",
    "connect_status",
    "connect_cancel",
    "network_status",
    "connection_stats",
    "license_status",
    "invite_create",
    "invite_connect",
    "view_next_frame",
    "view_cursor",
    "input_pointer_move",
    "input_press",
    "input_wheel",
    "chat_send",
    "chat_transcript",
    "clipboard_push",
    "clipboard_pull",
    "file_offer",
    "file_accept",
    "file_abort",
    "file_transfers",
    "audio_toggle",
    "recording_toggle",
    "record_request",
    "mic_toggle",
    "sas_request",
    "sas_available",
    "monitor_select",
    "monitors_list",
    "recordings_list",
    "recording_export",
    "audit_list",
    "audit_kinds",
    "audit_status",
    "audit_export",
    "audit_clear",
    "update_check",
    "update_install",
    "autostart_status",
    "autostart_set",
    "service_status",
    "service_set",
];

fn main() {
    let attrs = tauri_build::Attributes::new()
        .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS));

    // Default build reads capabilities/ (main.json, view.json) via tauri-
    // build's built-in default pattern. The `pilot` feature (debug-only
    // tauri-pilot integration, see Cargo.toml/main.rs) points instead at
    // capabilities-pilot/, which carries copies of main.json and pilot.json —
    // Tauri validates every capability file's permissions against the
    // plugins/commands actually compiled in, so pilot.json's `pilot:default`
    // permission must never be visible to a build that doesn't compile the
    // plugin, or the build fails. capabilities-pilot/main.json must stay in
    // sync with capabilities/main.json's `allow-*` list by hand (see its own
    // description field).
    let attrs = if cfg!(feature = "pilot") {
        println!("cargo:rerun-if-changed=capabilities-pilot");
        attrs.capabilities_path_pattern("./capabilities-pilot/**/*")
    } else {
        attrs
    };

    tauri_build::try_build(attrs)
        .unwrap_or_else(|error| panic!("failed to run tauri-build: {error}"));
}
