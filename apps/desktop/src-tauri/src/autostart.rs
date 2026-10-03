//! Starting with the user's session (ADR 0042).
//!
//! Three platform mechanisms, all of them **per user** and none of them
//! requiring elevation:
//!
//! | Platform | Where |
//! | --- | --- |
//! | Windows | a per-user logon task, `\Lumepeer\Autostart-<user>` (ADR 0140) |
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
//!
//! **On by default** (ADR 0103): the entry is written once, by the app
//! itself, the first time an installed copy runs — never by an installer,
//! which on Windows runs elevated and would write the wrong user's `HKCU`. A
//! marker file records that this has happened, so the paragraph above stays
//! true: off is off, and the next start does not argue.

use std::path::{Path, PathBuf};

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

    /// Turns autostart on once, the first time this installed copy ever runs
    /// (ADR 0103), and on macOS clears away a login item left pointing at an
    /// app that is gone (docs/bugs/12-service-lifecycle.md task 4; D6).
    ///
    /// **Nothing but the app itself can arrange the default.** On Windows the
    /// bundle is `installMode: perMachine`, so the NSIS hooks run elevated and
    /// the `HKCU` they would write is the administrator's hive rather than the
    /// hive of whoever later sits at the machine — the exact trap
    /// `packaging/deb-postinst.sh` sidesteps with `su -l "$target_user"`. On
    /// macOS `.dmg` is a drag-install with no hook of any kind. Only here is
    /// "the current user" not a guess.
    ///
    /// **Exactly once.** A marker file records that a first launch has
    /// happened, and while it exists this does nothing at all. That marker is
    /// the whole difference between "on by default" and "cannot be turned
    /// off": somebody who moves the switch to off (ADR 0042) is not argued
    /// with at the next start.
    ///
    /// Failures are logged and swallowed — this runs on every launch and must
    /// never be the reason the app fails to open.
    pub fn reconcile_first_launch(&self) {
        // macOS only. A drag-install has no uninstall hook either, so a copy
        // deleted from `/Applications` leaves a login item behind that nothing
        // else will ever remove; deb/rpm's `prerm` and the NSIS uninstaller do
        // this for the other two platforms.
        #[cfg(target_os = "macos")]
        platform::remove_stale_login_item();

        // Windows only: an older release's `Run` entry, which never started
        // the elevated client, becomes a task (ADR 0140).
        #[cfg(target_os = "windows")]
        if let Some(exe) = self.exe.as_deref() {
            platform::migrate_legacy(exe);
        }

        let Some(marker) = first_launch_marker() else {
            tracing::warn!("no per-user directory: cannot tell whether this is a first launch");
            return;
        };
        self.reconcile_with_marker(&marker);
    }

    /// [`Self::reconcile_first_launch`] against a marker path handed to it, so
    /// a test can give it one that is not this machine's own.
    fn reconcile_with_marker(&self, marker: &Path) {
        let Some(exe) = self.exe.as_deref() else {
            return;
        };
        if marker.exists() {
            return;
        }
        if !platform::is_enabled()
            && let Err(error) = platform::enable(exe)
        {
            tracing::warn!(%error, "could not turn on autostart at first launch");
        }
        // On a first run the directory this lives in may not exist yet:
        // nothing has been written into it.
        if let Some(parent) = marker.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(%error, "could not create {}", parent.display());
        }
        if let Err(error) = std::fs::write(marker, b"") {
            tracing::warn!(%error, "could not record that first launch ran");
        }
    }
}

/// Where the "first launch already ran" marker lives.
///
/// **The macOS path does not move.** Installed copies already have one next to
/// the login item, and reading a marker from a new path would look exactly
/// like a machine that has never run this app — turning autostart back on for
/// everybody who deliberately turned it off. The other two platforms have no
/// `LaunchAgents` directory to put it in and never had a marker to keep, so
/// theirs goes where the app keeps the rest of its per-user files.
fn first_launch_marker() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        return Some(
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{ENTRY_NAME}.first-launch")),
        );
    }
    lumepeer_runtime::config::config_dir().map(|dir| dir.join("autostart-first-launch"))
}

/// Windows: a per-user scheduled task, not the `Run` key (ADR 0140).
///
/// The client is `requireAdministrator` (ADR 0057), and Windows silently
/// skips a `Run` entry that needs elevation — so after a sign-in, including
/// one a guest made through the logon-screen host (ADR 0126), nothing was
/// hosting. A logon-triggered task with `HighestAvailable` starts it elevated
/// without a prompt. It is still per user: the trigger and the principal name
/// the account that turned it on, and nobody else's sign-in starts anything.
#[cfg(target_os = "windows")]
mod platform {
    use super::DISPLAY_NAME;
    use std::os::windows::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

    /// The per-user Run key the entry used to live in. Only read and cleared
    /// now: an entry left there is the one Windows never starts.
    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

    /// `CREATE_NO_WINDOW`: no console flashes up when the toggle is moved.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// `DOMAIN\user` of whoever this process runs as.
    fn account() -> Option<String> {
        let user = std::env::var("USERNAME").ok().filter(|u| !u.is_empty())?;
        Some(match std::env::var("USERDOMAIN") {
            Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
            _ => user,
        })
    }

    /// One task per account, so two people on one machine do not share it.
    fn task_name() -> Option<String> {
        let user = std::env::var("USERNAME").ok().filter(|u| !u.is_empty())?;
        Some(format!(r"\Lumepeer\Autostart-{user}"))
    }

    /// `schtasks.exe` by full path: this process is elevated, and a bare name
    /// would be resolved through a search path a user can write to.
    fn schtasks() -> Command {
        let root = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let mut command = Command::new(root.join("System32").join("schtasks.exe"));
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }

    fn task_exists() -> bool {
        task_name().is_some_and(|name| {
            schtasks()
                .args(["/Query", "/TN", &name])
                .output()
                .is_ok_and(|out| out.status.success())
        })
    }

    fn legacy_run_entry_exists() -> bool {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(RUN_KEY, KEY_READ)
            .is_ok_and(|key| key.get_value::<String, _>(DISPLAY_NAME).is_ok())
    }

    fn remove_legacy_run_entry() -> Result<(), String> {
        let Ok(key) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_WRITE)
        else {
            return Ok(());
        };
        match key.delete_value(DISPLAY_NAME) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("cannot remove the old startup entry: {error}")),
        }
    }

    fn xml_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    /// The task definition. `Priority` 4 is normal: the default of 7 would run
    /// a remote-desktop host at below-normal CPU and low I/O priority.
    pub(super) fn task_xml(account: &str, exe: &Path) -> String {
        let account = xml_escape(account);
        let exe = xml_escape(&exe.display().to_string());
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Starts Lumepeer when {account} signs in.</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{account}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{account}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command></Exec></Actions>
</Task>
"#
        )
    }

    /// A legacy `Run` entry counts as on: it is what the user chose, and the
    /// next start moves it into a task ([`migrate_legacy`]).
    pub fn is_enabled() -> bool {
        task_exists() || legacy_run_entry_exists()
    }

    pub fn enable(exe: &Path) -> Result<(), String> {
        let (Some(account), Some(name)) = (account(), task_name()) else {
            return Err("cannot tell which account this process runs as".to_owned());
        };
        // UTF-16 with a BOM is the encoding schtasks reads without argument.
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(
            task_xml(&account, exe)
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        let file =
            std::env::temp_dir().join(format!("lumepeer-autostart-{}.xml", std::process::id()));
        std::fs::write(&file, bytes)
            .map_err(|error| format!("cannot write the task definition: {error}"))?;
        let out = schtasks()
            .args(["/Create", "/F", "/TN", &name, "/XML"])
            .arg(&file)
            .output();
        let _ = std::fs::remove_file(&file);
        let out = out.map_err(|error| format!("cannot run schtasks: {error}"))?;
        if !out.status.success() {
            return Err(format!(
                "cannot create the startup task: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        remove_legacy_run_entry()
    }

    pub fn disable() -> Result<(), String> {
        if let Some(name) = task_name().filter(|_| task_exists()) {
            let out = schtasks()
                .args(["/Delete", "/F", "/TN", &name])
                .output()
                .map_err(|error| format!("cannot run schtasks: {error}"))?;
            if !out.status.success() {
                return Err(format!(
                    "cannot remove the startup task: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        remove_legacy_run_entry()
    }

    /// Moves an entry an older release wrote into the `Run` key into a task.
    pub fn migrate_legacy(exe: &Path) {
        if legacy_run_entry_exists()
            && let Err(error) = enable(exe)
        {
            tracing::warn!(%error, "could not move autostart from the Run key to a task");
        }
    }
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
                .map_or_else(|| home.join(".config"), PathBuf::from);
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

    /// Pulls the path back out of the `<string>` inside `plist`'s
    /// `ProgramArguments` array.
    ///
    /// Just enough of an XML "parser" for a file this module wrote itself in
    /// the first place: `body` is never anything but the exact template
    /// `plist` produces, never input from outside the app.
    #[cfg(target_os = "macos")]
    fn recorded_target(body: &str) -> Option<&str> {
        let after_array = &body[body.find("<array>")? + "<array>".len()..];
        let start = after_array.find("<string>")? + "<string>".len();
        let end = after_array[start..].find("</string>")?;
        Some(after_array[start..start + end].trim())
    }

    /// Removes a login item whose target no longer exists — most plausibly
    /// this exact app, deleted from `/Applications` by dragging it to the
    /// Trash, with nothing left behind to have disabled it first — rather
    /// than leaving it to fail silently at every future login.
    ///
    /// See [`super::Autostart::reconcile_first_launch`].
    #[cfg(target_os = "macos")]
    pub fn remove_stale_login_item() {
        let Some(path) = entry_path() else {
            return;
        };
        let stale = std::fs::read_to_string(&path)
            .ok()
            .and_then(|body| recorded_target(&body).map(str::to_owned))
            .is_some_and(|target| !Path::new(&target).exists());
        if !stale {
            return;
        }
        tracing::warn!("the login item points at a file that no longer exists; removing it");
        if let Err(error) = disable() {
            tracing::warn!(%error, "could not remove the stale login item");
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    mod tests {
        use super::recorded_target;

        /// Pulls the path back out of a plist this same module wrote,
        /// exactly the shape `reconcile_first_launch` reads back at every
        /// startup.
        #[test]
        fn reads_the_path_written_into_the_program_arguments_array() {
            let body = super::plist(std::path::Path::new("/Applications/Lumepeer.app/lumepeer"));
            assert_eq!(
                recorded_target(&body),
                Some("/Applications/Lumepeer.app/lumepeer")
            );
        }

        /// A file that is not a plist this module wrote — empty, or missing
        /// the array entirely — is "no target", not a panic.
        #[test]
        fn is_none_without_a_program_arguments_array() {
            assert_eq!(recorded_target(""), None);
            assert_eq!(recorded_target("<plist><dict/></plist>"), None);
        }
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

    /// The task starts this exe elevated, as the account that turned it on,
    /// and only at that account's sign-in (ADR 0140).
    #[cfg(target_os = "windows")]
    #[test]
    fn the_task_starts_elevated_for_its_own_account_only() {
        let xml = platform::task_xml(
            r"PC\a&b",
            Path::new(r"C:\Program Files\Lumepeer\lumepeer-desktop.exe"),
        );
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains(r"<LogonTrigger><Enabled>true</Enabled><UserId>PC\a&amp;b</UserId>"));
        assert!(xml.contains(r"<Command>C:\Program Files\Lumepeer\lumepeer-desktop.exe</Command>"));
    }

    /// The marker is the whole difference between "on by default" and "cannot
    /// be turned off": with one on disk, a start turns nothing on.
    #[test]
    fn an_existing_marker_turns_nothing_on() {
        // Only meaningful on a machine with no entry of its own — and a
        // developer who has one keeps it, exactly as in the test above.
        let autostart = Autostart::for_this_app();
        if autostart.is_enabled() {
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "lumepeer-first-launch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("autostart-first-launch");
        std::fs::write(&marker, b"").unwrap();

        autostart.reconcile_with_marker(&marker);

        assert!(
            !autostart.is_enabled(),
            "a marker on disk must stop first-launch from writing an entry"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
