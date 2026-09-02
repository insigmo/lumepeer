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
    "file_accept",
    "file_abort",
    "file_transfers",
    "audio_toggle",
    "recording_toggle",
    "record_request",
    "mic_toggle",
    "sas_request",
    "monitor_select",
    "monitors_list",
    "view_set_scale",
    "host_display_modes",
    "host_display_set_mode",
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
    "host_bar_expand",
    "host_bar_focus_main",
];

/// The Windows application manifest, replacing tauri-build's default.
///
/// Two changes from the default, and only two:
///
/// - `requestedExecutionLevel level="requireAdministrator"`. Lumepeer injects
///   input with `SendInput`, and UIPI silently drops input a medium-integrity
///   process aims at a higher-integrity window — every window an elevated app
///   owns (`services.msc`, `regedit`, Task Manager, an installer's own UI).
///   Running the client at high integrity is what lets a guest drive those.
///   It does **not** reach the secure desktop (the UAC prompt itself, the lock
///   screen): `Winsta0\Winlogon` is a different desktop object that no amount
///   of elevation puts this process's thread onto — that half goes through the
///   `LocalSystem` helper (ADR 0043, ADR 0049, ADR 0056). See ADR 0057 for why
///   always-elevated was chosen over relaunch-on-demand and what it costs (a
///   UAC prompt at every launch; no drag-and-drop from a non-elevated
///   Explorer).
/// - Nothing else. The `Microsoft.Windows.Common-Controls` v6 dependency is
///   carried over verbatim from tauri-build's own default manifest, because
///   dropping it breaks tauri's dialog APIs (tauri-build's docs warn of this)
///   and this app depends on `tauri-plugin-dialog`.
const APP_MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*" />
    </dependentAssembly>
  </dependency>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>"#;

fn main() {
    let attrs = tauri_build::Attributes::new()
        .windows_attributes(tauri_build::WindowsAttributes::new().app_manifest(APP_MANIFEST))
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
