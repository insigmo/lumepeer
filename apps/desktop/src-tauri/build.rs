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
    "session_ask_host",
    "address_book_list",
    "address_book_upsert",
    "address_book_remove",
    "address_book_set_trusted",
    "unattended_status",
    "unattended_set_password",
    "unattended_disable",
    "unattended_set_totp",
    "unattended_set_role",
    "unattended_set_logon_screen",
    "unattended_set_accept_after_sign_in",
    "unattended_submit",
    "connection_history",
    "history_connect",
    "history_remove",
    "history_forget_password",
    "guest_key_hint",
    "history_set_trusted",
    "history_probe",
    "history_check",
    "connect_status",
    "connect_cancel",
    "report_decoder_codecs",
    "network_status",
    "connection_stats",
    "license_status",
    "license_activate",
    "license_deactivate",
    "invite_create",
    "invite_current",
    "invite_connect",
    "view_next_frame",
    "view_next_chunk",
    "view_cursor",
    "view_media_stats",
    "input_pointer_move",
    "input_press",
    "input_wheel",
    "chat_send",
    "chat_transcript",
    "chat_mark_read",
    "clipboard_push",
    "clipboard_pull",
    "file_accept",
    "file_abort",
    "file_transfers",
    "local_dir_list",
    "remote_dir_list",
    "remote_dir_status",
    "remote_download",
    "remote_upload",
    "files_window_open",
    "remote_file_op",
    "local_file_op",
    "local_roots",
    "tunnel_open",
    "tunnel_close_all",
    "tunnel_set_target",
    "tunnel_status",
    "terminal_open",
    "terminal_input",
    "terminal_resize",
    "terminal_close",
    "terminal_poll",
    "reboot_request",
    "reboot_pending",
    "reboot_cancel",
    "audio_toggle",
    "recording_toggle",
    "record_request",
    "mic_toggle",
    "sas_request",
    "monitor_select",
    "monitors_list",
    "view_set_scale",
    "view_set_size",
    "view_keyboard_grab",
    "host_display_modes",
    "host_display_set_mode",
    "host_encoders",
    "host_encoder_select",
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
    "mac_permissions",
    "mac_permission_request",
    "host_bar_expand",
    "host_bar_focus_main",
    "host_bar_open_chat",
    // The shield button: this binary again, elevated, as the access-settings
    // window (ADR 0169 §5).
    "access_settings_open",
    // "Start the service" where its session host does not answer (ADR 0169
    // §2): the service binary elevated, one prompt.
    "service_start",
];

/// The Windows application manifest, replacing tauri-build's default.
///
/// Two changes from the default, and only two:
///
/// - `requestedExecutionLevel level="asInvoker"` (ADR 0169). The client starts
///   with whatever token the person has, no UAC prompt, and on a standard
///   account too, and drag-and-drop from Explorer reaches it. It needs nothing
///   more, because on Windows a release client never hosts: this machine's
///   host is this same binary run by the helper service as
///   `--session-host`, a `LocalSystem` process on the person's desktop. That
///   process is what drives elevated windows and a `VMware` guest, keeps the
///   access policy where only administrators can write it, and asks the
///   helper for the secure desktop. Administrator rights are spent once on
///   installing the service, and again only when the access policy changes
///   (this binary elevated as `--access-settings`, one prompt). ADR 0057 shipped
///   `requireAdministrator` instead, at the cost of a prompt at every launch;
///   ADR 0169 §2 says why that is gone.
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
        <requestedExecutionLevel level="asInvoker" uiAccess="false" />
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
