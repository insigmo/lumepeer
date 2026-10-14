//! The macOS permissions screen's two questions: where each permission
//! stands, and "ask for this one" (`lumepeer_media::capture::macos_permissions`
//! does the asking).
//!
//! An installer cannot switch any of them on — they are the person's to give
//! in System Settings — so the most the app can do is ask for all three
//! before the first session instead of in the middle of it, and take the
//! person to the right place in System Settings when a prompt is no longer
//! on offer. Every other platform answers "nothing to ask".

use serde::{Deserialize, Serialize};

/// Where one permission stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "only macOS has these permissions to report")
)]
pub enum PermissionState {
    /// On for this app.
    Granted,
    /// Not on yet; asking may still show the system's own prompt.
    NotGranted,
    /// Refused: only System Settings can change it now.
    Denied,
}

/// The three permissions a Mac running Lumepeer needs.
#[derive(Debug, Clone, Serialize)]
pub struct MacPermissions {
    /// Screen & System Audio Recording: this Mac's picture and sound, as a
    /// host.
    pub screen_recording: PermissionState,
    /// Accessibility: a guest's mouse and keyboard on this Mac.
    pub accessibility: PermissionState,
    /// Microphone: this Mac's microphone, sent to a host while its button is
    /// on.
    pub microphone: PermissionState,
}

/// One permission the screen can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MacPermission {
    ScreenRecording,
    Accessibility,
    Microphone,
}

/// Where the three permissions stand right now, or `None` off macOS.
#[must_use]
#[cfg_attr(
    target_os = "macos",
    allow(
        clippy::unnecessary_wraps,
        reason = "every other platform answers `None`"
    )
)]
pub fn read() -> Option<MacPermissions> {
    #[cfg(target_os = "macos")]
    {
        use lumepeer_media::capture::macos_permissions as tcc;

        let on = |granted: bool| {
            if granted {
                PermissionState::Granted
            } else {
                PermissionState::NotGranted
            }
        };
        Some(MacPermissions {
            screen_recording: on(tcc::screen_recording()),
            accessibility: on(tcc::accessibility()),
            microphone: match tcc::microphone() {
                tcc::Microphone::Authorized => PermissionState::Granted,
                tcc::Microphone::NotDetermined => PermissionState::NotGranted,
                tcc::Microphone::Denied => PermissionState::Denied,
            },
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Asks for `permission`: the system's own prompt where it still has one to
/// show, and System Settings opened at the permission's list where the
/// person has to switch it on there.
///
/// Screen Recording and Accessibility get both: their request calls show a
/// prompt only the first time ever, but each also puts Lumepeer in the list
/// the person then switches on. The microphone's prompt is a real Allow /
/// Don't Allow, so System Settings is only for a microphone already refused.
pub fn request(permission: MacPermission) {
    #[cfg(target_os = "macos")]
    {
        use lumepeer_media::capture::macos_permissions as tcc;

        let pane = match permission {
            MacPermission::ScreenRecording => {
                tcc::request_screen_recording();
                "Privacy_ScreenCapture"
            }
            MacPermission::Accessibility => {
                tcc::request_accessibility();
                "Privacy_Accessibility"
            }
            MacPermission::Microphone => {
                if tcc::microphone() == tcc::Microphone::NotDetermined {
                    tcc::request_microphone();
                    return;
                }
                "Privacy_Microphone"
            }
        };
        let url = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
        if let Err(error) = std::process::Command::new("/usr/bin/open")
            .arg(&url)
            .spawn()
        {
            tracing::warn!(%error, %url, "cannot open System Settings");
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = permission;
    }
}
