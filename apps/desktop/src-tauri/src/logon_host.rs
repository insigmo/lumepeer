//! This application, hosting the machine's logon screen (ADR 0126).
//!
//! Launched by the helper service with one argument
//! ([`lumepeer_service::LOGON_HOST_ARG`]) into the console session's
//! `WinSta0\Winlogon` desktop, as `LocalSystem`, whenever nobody is signed in
//! there — after a reboot, after a sign-out. It is the whole host, not a part
//! of one: the same runtime the client runs, with the identity, the device
//! password, the address book and the audit log of the account that turned
//! the feature on. So a guest dials the same saved host it always dials, gets
//! in with the device password it remembers, sees the logon screen and types
//! into it.
//!
//! Three things differ from the client, and each is the logon screen's rather
//! than a choice:
//!
//! - **Nobody is there to ask.** [`LogonScreen::attendance`] is always
//!   unattended, so the device password is the only way in (ADR 0085 §2), and
//!   every admission is audited as a login to an empty machine (ADR 0088 §3).
//! - **The screen is the secure desktop.** A keystroke therefore takes
//!   `secure_desktop_input` on top of the ordinary input grant, exactly as on a
//!   UAC prompt (ADR 0061); a full-control role carries it.
//! - **There is no banner.** The only desktop to draw one on is `Winlogon`
//!   (ADR 0088 §3, the known gap). The person who signs in gets the client's
//!   own, because the moment they do the service asks this process to give the
//!   host role up, it exits, and the client takes over. Its guests are told to
//!   dial again, and a guest still here is what the service starts the client
//!   for (ADR 0164).
//!
//! It refuses — with [`LOGON_HOST_EXIT_NOT_ENABLED`] — unless the feature is
//! on, the owner's store already holds an identity (a new one would be a
//! different host from the one every guest saved) and a device password is
//! set: with nobody to answer a dialog, a host without one would admit nobody
//! and only listen. Everything after those checks is [`crate::system_host`],
//! which the session host of ADR 0169 shares.

use std::sync::Arc;

use lumepeer_core::consent::HostAttendance;
use lumepeer_net::keystore::{IDENTITY_ENTRY, UNATTENDED_PASSWORD_ENTRY};
use lumepeer_runtime::view::{ViewSurface, ViewWindows};
use lumepeer_service::{
    LOGON_HOST_EXIT_HANDED_OVER, LOGON_HOST_EXIT_NOT_ENABLED, LOGON_HOST_EXIT_ROLE_TAKEN,
};

use crate::system_host::{self, EXIT_FAILED, Hosting};

/// Where the relay measurement of ADR 0098 is kept, in the owner's directory
/// beside the stores it hosts with.
const RELAY_CACHE_FILE: &str = "relays-logon-host.json";

/// Runs this process as the logon host and never returns.
pub fn run() -> ! {
    system_host::run(
        lumepeer_service::log::LOGON_HOST_LOG_FILE,
        "starting as this machine's logon-screen host (ADR 0126)",
        host,
    )
}

/// Hosts until the service asks for the role back; returns the exit code.
fn host() -> u32 {
    if !system_host::is_local_system() {
        return EXIT_FAILED;
    }
    let Some(owner) = lumepeer_service::program_data::logon_host_owner() else {
        tracing::info!("hosting the logon screen is turned off");
        return LOGON_HOST_EXIT_NOT_ENABLED;
    };
    let Some(placement) = crate::placement::for_system_host(&owner) else {
        tracing::warn!("the account that turned this on has no store to host with");
        return LOGON_HOST_EXIT_NOT_ENABLED;
    };
    if !matches!(placement.keystore.load_secret(IDENTITY_ENTRY), Ok(Some(_))) {
        tracing::warn!(
            "the owner's store has no identity yet; a new one would be a host no guest has saved"
        );
        return LOGON_HOST_EXIT_NOT_ENABLED;
    }
    if !matches!(
        placement.keystore.load_secret(UNATTENDED_PASSWORD_ENTRY),
        Ok(Some(_))
    ) {
        tracing::info!(
            "no device password is set, so nobody could be let in at the logon screen; not hosting"
        );
        return LOGON_HOST_EXIT_NOT_ENABLED;
    }
    system_host::host(Hosting {
        placement,
        screen: Arc::new(LogonScreen),
        relay_cache_file: RELAY_CACHE_FILE,
        serving: "the logon screen",
        role_taken: LOGON_HOST_EXIT_ROLE_TAKEN,
        leaving: exit_code_for,
    })
}

/// The exit code of a logon host that leaves with `guests` on it: leaving
/// with a guest asks the service for the owner's client (ADR 0164).
const fn exit_code_for(guests: usize) -> u32 {
    if guests > 0 {
        LOGON_HOST_EXIT_HANDED_OVER
    } else {
        0
    }
}

/// The logon host's screen seam: no windows, nobody present, and a screen
/// that is the secure desktop.
#[derive(Debug)]
struct LogonScreen;

impl ViewWindows for LogonScreen {
    /// The logon host never views another machine, so there is nothing to
    /// open.
    fn open(
        &self,
        label: &str,
        _peer_label: &str,
        _host_label: &str,
        _input: bool,
        _surface: ViewSurface,
    ) {
        tracing::warn!(window = %label, "the logon host has no view windows");
    }

    fn close(&self, _label: &str) {}

    /// No bar: the only desktop to put one on is `Winlogon` (ADR 0088 §3).
    fn set_host_bar(&self, _visible: bool) {}

    fn attendance(&self) -> HostAttendance {
        HostAttendance::Unattended
    }

    fn on_secure_desktop(&self) -> bool {
        true
    }

    fn nobody_signed_in(&self) -> bool {
        true
    }

    /// Nobody is signed in, so there is nobody to read, write or start a
    /// shell as: a guest's file work and shells are refused here rather than
    /// done as `LocalSystem` (ADR 0169 §6).
    fn person(&self) -> lumepeer_runtime::person::Person {
        lumepeer_runtime::person::Person::Nobody
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Leaving with a guest asks the service for the client; leaving with
    /// nobody is an ordinary exit.
    #[test]
    fn a_guest_on_the_way_out_asks_for_the_client() {
        assert_eq!(exit_code_for(1), LOGON_HOST_EXIT_HANDED_OVER);
        assert_eq!(exit_code_for(0), 0);
    }

    /// The logon screen asks nobody, is always the secure desktop, and is an
    /// empty machine — the three answers the actor's admission, input gate and
    /// audit read.
    #[test]
    fn the_logon_screen_is_unattended_secure_and_empty() {
        let screen = LogonScreen;
        assert_eq!(screen.attendance(), HostAttendance::Unattended);
        assert!(screen.on_secure_desktop());
        assert!(screen.nobody_signed_in());
        assert!(
            matches!(screen.person(), lumepeer_runtime::person::Person::Nobody),
            "nobody to do a guest's files or shell as (ADR 0169 §6)"
        );
        assert!(
            !screen.injects_as_system(),
            "its input goes to the secure desktop"
        );
    }
}
