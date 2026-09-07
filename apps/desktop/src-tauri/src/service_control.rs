//! Registering and observing the privileged helper service (ADR 0043).
//!
//! The service itself is `crates/service`; this is the unprivileged half the
//! app drives. Three rules shape it:
//!
//! - **Nothing here is privileged.** Reading the service's state needs no
//!   rights; changing it does, and the way this asks for them is to launch the
//!   *service binary* elevated with one flag. No shell command line is built
//!   out of anything, so there is no quoting bug to turn into "run whatever
//!   this path says as SYSTEM".
//! - **Running is what matters.** "Installed" is a fact about the registry;
//!   "reachable" is a fact about whether Ctrl+Alt+Del will work. [`state`]
//!   separates the two, because a service that is installed and stopped looks
//!   like a working one otherwise.
//! - **It can always be removed.** Uninstalling Lumepeer uninstalls the
//!   service (`installer-hooks.nsh`); a privileged service a person cannot get
//!   rid of is the thing this project must not ship. There is no switch for it
//!   inside the app any more, because "off" was never an answer anyone had a
//!   reason to give — see [`ensure_installed`].

/// Where the service is, as far as this machine can tell without elevating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// There is no helper service to install here — either this platform has
    /// no privileged action for one to hold (ADR 0043: only Windows does), or
    /// this build was assembled without the sidecar beside it. Either way the
    /// panel offers nothing rather than a button that cannot work.
    Unsupported,
    /// Not registered with the service control manager.
    #[cfg(target_os = "windows")]
    NotInstalled,
    /// Registered, but not answering. Ctrl+Alt+Del falls back to needing an
    /// elevated client.
    #[cfg(target_os = "windows")]
    Stopped,
    /// Registered and answering on its endpoint.
    #[cfg(target_os = "windows")]
    Running,
}

/// What this machine's helper service is doing right now.
///
/// Reads the machine without elevating; [`ensure_installed`] acts on it.
#[must_use]
pub fn state() -> ServiceState {
    #[cfg(not(target_os = "windows"))]
    {
        ServiceState::Unsupported
    }
    #[cfg(target_os = "windows")]
    {
        // Reachability first: it is the question the SAS button actually
        // depends on, and it needs no subprocess.
        if lumepeer_service::client::is_reachable() {
            return ServiceState::Running;
        }
        if windows_impl::is_registered() {
            return ServiceState::Stopped;
        }
        // Nothing installed, and nothing to install *from*: a build without
        // the sidecar beside it cannot offer this at all, which is a different
        // answer from "you have not installed it yet".
        if windows_impl::service_exe().is_none() {
            return ServiceState::Unsupported;
        }
        ServiceState::NotInstalled
    }
}

/// Registers and starts the helper service if this machine is missing it.
///
/// This used to be an Install/Remove pair in the settings panel (ADR 0063).
/// The service is not a permission — it admits nobody, and holds exactly one
/// capability — so the switch was a question the user had no basis to answer,
/// and the only symptom of answering it wrong was a Ctrl+Alt+Del button that
/// silently did nothing. It is on, always, and this is what keeps it that way.
///
/// The third rule at the top of this file still holds, one level up: the
/// installer registers the service (`installer-hooks.nsh`) and the
/// **uninstaller removes it**, so removing the app removes the service. This
/// covers what the installer cannot — a development run, an installation that
/// could not finish its hook, and a service somebody stopped by hand.
///
/// Nothing is prompted for: the client already runs elevated (ADR 0057), so
/// the elevation this needs is the one it was launched with. Blocking, and a
/// failure is logged rather than raised — a missing service degrades
/// Ctrl+Alt+Del, not the app (§18).
pub fn ensure_installed() {
    match state() {
        // Nothing on this machine for a helper to hold.
        ServiceState::Unsupported => {}
        // Already answering: `--install` would only stop and restart it.
        #[cfg(target_os = "windows")]
        ServiceState::Running => {}
        #[cfg(target_os = "windows")]
        ServiceState::NotInstalled | ServiceState::Stopped => {
            match windows_impl::elevate("--install") {
                Ok(()) => tracing::info!("registered the Ctrl+Alt+Del helper service"),
                Err(error) => tracing::warn!(%error, "cannot register the helper service"),
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use std::os::windows::process::CommandExt as _;
    use std::path::PathBuf;
    use std::process::Command;

    /// `CREATE_NO_WINDOW`: a GUI app must not flash a console at the user
    /// every time it asks a question about its own service.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// `sc query` for a service the machine does not have.
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

    /// The sidecar next to the running executable.
    ///
    /// Tauri stages `externalBin` beside the main binary with the target
    /// triple stripped, and `cargo build` puts both in the same directory, so
    /// one rule covers the installed app and a development run alike.
    pub fn service_exe() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.join("lumepeer-service.exe");
        path.is_file().then_some(path)
    }

    /// Whether the service control manager knows this service.
    pub fn is_registered() -> bool {
        let Ok(output) = Command::new("sc.exe")
            .args(["query", lumepeer_service::SERVICE_NAME])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        else {
            return false;
        };
        // 1060 is "no such service". Anything else — running, stopped, paused,
        // or an access error — means something is registered under the name,
        // and reporting it as absent would offer to install a second one.
        output.status.code() != Some(ERROR_SERVICE_DOES_NOT_EXIST)
    }

    /// Runs the service binary elevated with one flag.
    ///
    /// `Start-Process -Verb RunAs` is what raises the consent prompt. The only
    /// thing interpolated into the script is the sidecar's own path, which
    /// comes from `current_exe` rather than from anywhere a user or a peer can
    /// write, and single quotes in it are doubled so it cannot end the string
    /// it sits in.
    pub fn elevate(flag: &str) -> Result<(), String> {
        let exe = service_exe()
            .ok_or_else(|| "the helper service is not installed next to this app".to_owned())?;
        let path = exe.to_string_lossy().replace('\'', "''");
        let script =
            format!("Start-Process -FilePath '{path}' -ArgumentList '{flag}' -Verb RunAs -Wait");
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("cannot ask for administrator rights: {error}"))?;
        if output.status.success() {
            return Ok(());
        }
        // The overwhelmingly common failure is the user closing the consent
        // prompt, which is a decision, not a fault. Saying so beats reporting
        // a Win32 error nobody asked to see.
        Err("the change needs administrator rights, and they were not given".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state is always an answer, on every platform, without elevating and
    /// without panicking: the settings panel asks for it on every render.
    #[test]
    fn the_state_is_always_an_answer() {
        let observed = state();
        if !cfg!(target_os = "windows") {
            assert_eq!(
                observed,
                ServiceState::Unsupported,
                "no platform but Windows has a privileged action for a helper to hold"
            );
        }
    }

    /// Off Windows there is nothing to register, and start-up says so by
    /// doing nothing rather than by shelling out to a tool that is not there.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn registering_is_skipped_where_there_is_no_service() {
        ensure_installed();
    }
}
