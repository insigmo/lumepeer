//! Starting with the user's session (ADR 0042).
//!
//! Three platform mechanisms, all of them **per user** and none of them
//! requiring elevation:
//!
//! | Platform | Where |
//! | --- | --- |
//! | Windows | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` |
//! | macOS | `~/Library/LaunchAgents/io.insigmo.lumepeer.plist` |
//! | Linux | `~/.config/autostart/io.insigmo.lumepeer.desktop` |
//!
//! `HKLM` and a system service are deliberately not here. Those start the app
//! before anybody logs in, which is a different feature with different stakes
//! (`docs/tasks/14-release-infrastructure.md`, task 4) and is not something a
//! toggle in a settings panel should be able to arrange.
//!
//! **Autostart permits nothing.** The app comes up and waits for consent
//! exactly as it does when a person launches it: no session exists, no grant
//! is implied, and a guest still has to be let in. Permanent admission is
//! `unattended` (ADR 0033) and is turned on separately, on purpose.
//!
//! Turning it off removes the entry completely — the registry value is
//! deleted, not blanked; the plist and the `.desktop` file are unlinked, not
//! left with a disabled flag. Software you cannot uninstall from its own
//! settings is what this app must not be.

use std::path::PathBuf;

/// Name the entry is written under, on every platform.
const ENTRY_NAME: &str = "io.insigmo.lumepeer";

/// Human-facing name of the registry value and the `.desktop` entry.
const DISPLAY_NAME: &str = "Lumepeer";

/// The autostart entry of this installation.
#[derive(Debug, Clone)]
pub struct Autostart {
    /// Executable the entry points at, or `None` when this process cannot say
    /// where it lives — in which case autostart is reported as unavailable
    /// rather than pointed at a guess.
    exe: Option<PathBuf>,
}

impl Autostart {
    /// The entry for the currently running executable.
    #[must_use]
    pub fn for_this_app() -> Self {
        Self {
            exe: std::env::current_exe().ok(),
        }
    }

    /// Whether this platform can arrange autostart at all.
    #[must_use]
    pub const fn available(&self) -> bool {
        self.exe.is_some()
    }

    /// Whether the entry exists right now.
    ///
    /// Reads the real mechanism every time rather than remembering what this
    /// process last wrote: the user may have removed it by hand between runs,
    /// and a toggle that shows a stale state is worse than no toggle.
    ///
    /// A platform that will not answer reads as "not enabled". The question is
    /// what this machine does at sign-in, and the answer an unreadable registry
    /// key or an unreachable home directory supports is "nothing".
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.exe.is_some() && platform::is_enabled()
    }

    /// Adds or removes the entry.
    ///
    /// Removing an absent entry succeeds: the post-condition is "there is no
    /// autostart entry", not "an entry was deleted".
    ///
    /// # Errors
    /// A description of what the platform refused, never a panic.
    pub fn set(&self, enabled: bool) -> Result<(), String> {
        let Some(exe) = self.exe.as_deref() else {
            return Err("this process cannot locate its own executable".to_owned());
        };
        if enabled {
            platform::enable(exe)
        } else {
            platform::disable()
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{DISPLAY_NAME, ENTRY_NAME};
    use std::path::Path;

    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

    /// The per-user Run key. `HKLM`'s equivalent is deliberately untouched:
    /// writing there needs elevation and starts the app for every account on
    /// the machine, neither of which a settings toggle may decide.
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    pub fn is_enabled() -> bool {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        // No Run key at all is a machine that has never had a startup entry,
        // which is the same answer as "not enabled".
        hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ)
            .is_ok_and(|key| key.get_value::<String, _>(DISPLAY_NAME).is_ok())
    }

    pub fn enable(exe: &Path) -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu
            .create_subkey_with_flags(RUN_KEY, KEY_WRITE)
            .map_err(|error| format!("cannot open the per-user Run key: {error}"))?;
        // Quoted: a path with a space in it is otherwise read as a command
        // plus arguments, and `C:\Program Files\...` is the normal case.
        let command = format!("\"{}\"", exe.display());
        key.set_value(DISPLAY_NAME, &command)
            .map_err(|error| format!("cannot write the startup entry: {error}"))
    }

    pub fn disable() -> Result<(), String> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_WRITE) else {
            return Ok(());
        };
        match key.delete_value(DISPLAY_NAME) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("cannot remove the startup entry: {error}")),
        }
    }

    /// Unused off the other platforms; kept so every arm has the same shape.
    #[allow(dead_code, reason = "the name is used by the other platform arms")]
    const _: &str = ENTRY_NAME;
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::{DISPLAY_NAME, ENTRY_NAME};
    use std::path::{Path, PathBuf};

    /// The file that has to exist for this app to start with the session.
    fn entry_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        if cfg!(target_os = "macos") {
            Some(
                home.join("Library")
                    .join("LaunchAgents")
                    .join(format!("{ENTRY_NAME}.plist")),
            )
        } else {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config"));
            Some(base.join("autostart").join(format!("{ENTRY_NAME}.desktop")))
        }
    }

    pub fn is_enabled() -> bool {
        entry_path().is_some_and(|path| path.exists())
    }

    pub fn enable(exe: &Path) -> Result<(), String> {
        let path = entry_path().ok_or_else(|| "no home directory".to_owned())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let body = if cfg!(target_os = "macos") {
            plist(exe)
        } else {
            desktop_entry(exe)
        };
        std::fs::write(&path, body)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))
    }

    pub fn disable() -> Result<(), String> {
        let Some(path) = entry_path() else {
            return Ok(());
        };
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
        }
    }

    /// A `launchd` *agent*: it runs as the logged-in user, in that user's
    /// session. A daemon in `/Library/LaunchDaemons` would run before login
    /// and as root, which is the separate feature this module refuses.
    fn plist(exe: &Path) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{ENTRY_NAME}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#,
            exe = exe.display()
        )
    }

    /// A freedesktop autostart entry. `X-GNOME-Autostart-enabled` is written
    /// explicitly so a desktop that remembers a previous "disabled" state does
    /// not silently ignore a freshly written file.
    fn desktop_entry(exe: &Path) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={DISPLAY_NAME}\n\
             Exec=\"{exe}\"\n\
             Terminal=false\n\
             X-GNOME-Autostart-enabled=true\n",
            exe = exe.display()
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "a failed assumption must fail the test")]

    use super::*;

    /// A process that cannot find its own executable reports autostart as
    /// unavailable rather than writing an entry pointing at a guess.
    #[test]
    fn without_an_executable_path_it_refuses_to_write() {
        let autostart = Autostart { exe: None };
        assert!(!autostart.available());
        assert!(!autostart.is_enabled());
        assert!(autostart.set(true).is_err());
    }

    /// Turning it off when it is already off succeeds: the post-condition is
    /// "no entry exists", not "an entry was removed".
    #[test]
    fn disabling_an_absent_entry_succeeds() {
        // Only meaningful when this machine has no entry to begin with, which
        // is the state a test runner is in; if a developer has one, skip
        // rather than delete it out from under them.
        let autostart = Autostart::for_this_app();
        if autostart.is_enabled() {
            return;
        }
        assert!(autostart.set(false).is_ok());
        assert!(!autostart.is_enabled());
    }
}
