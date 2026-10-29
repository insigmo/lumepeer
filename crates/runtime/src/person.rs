//! Who a host's file work and shells run as (ADR 0169 §6).
//!
//! A desktop client is the person at the machine, so what it does for a guest
//! it does as itself — [`Person::ThisProcess`], every host before ADR 0169.
//! The session host is `LocalSystem` acting for whoever is signed in, and must
//! do a guest's file work and start a guest's shell **as that person**, never
//! as itself: [`Person::SignedIn`]. The logon host has nobody to act as, and
//! refuses both: [`Person::Nobody`].
//!
//! The actor asks the host's screen seam
//! ([`crate::view::ViewWindows::person`]) every time, so the answer is
//! always the seam's, never a copy taken at start-up.

use std::sync::Arc;

use lumepeer_net::run_as::{DiskAs, RunAs};
use lumepeer_terminal::{Shell, ShellError, ShellSize};

/// The signed-in person a `LocalSystem` host acts for.
///
/// [`RunAs::run`] is a guest's file work, in the identity ADR 0169 §6 gives
/// it (the elevated half of a split token, the person's own otherwise);
/// [`Self::spawn_shell`] is a guest's shell, with the person's own token.
pub trait SignedIn: RunAs {
    /// Starts the person's shell at `size`, with their own token.
    ///
    /// # Errors
    /// As [`Shell::spawn`]: [`ShellError::CannotDropPrivileges`] when there
    /// is no token of theirs to start it with.
    fn spawn_shell(&self, size: ShellSize) -> Result<Shell, ShellError>;
}

/// Who a host's file work and shells run as.
#[derive(Debug, Clone, Default)]
pub enum Person {
    /// This process: it is the person at the machine.
    #[default]
    ThisProcess,
    /// A `LocalSystem` host acting for the person signed in.
    SignedIn(Arc<dyn SignedIn>),
    /// Nobody is signed in: file work and shells are refused.
    Nobody,
}

impl Person {
    /// Who the disk is touched as for a guest.
    #[must_use]
    pub fn disk(&self) -> DiskAs {
        match self {
            Self::ThisProcess => DiskAs::ThisProcess,
            Self::SignedIn(person) => DiskAs::Person(Arc::clone(person) as Arc<dyn RunAs>),
            Self::Nobody => DiskAs::Nobody,
        }
    }

    /// Starts a guest's shell as this person (ADR 0079, ADR 0169 §6).
    ///
    /// # Errors
    /// As [`Shell::spawn`]; [`ShellError::CannotDropPrivileges`] when nobody
    /// is signed in, since a shell belonging to nobody is a `LocalSystem` one.
    pub fn spawn_shell(&self, size: ShellSize) -> Result<Shell, ShellError> {
        match self {
            Self::ThisProcess => Shell::spawn(size),
            Self::SignedIn(person) => person.spawn_shell(size),
            Self::Nobody => {
                tracing::warn!("refusing a remote shell: nobody is signed in to run it as");
                Err(ShellError::CannotDropPrivileges)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nobody signed in starts no shell, and every disk call is refused.
    #[test]
    fn nobody_signed_in_is_a_refusal_everywhere() {
        assert_eq!(
            Person::Nobody
                .spawn_shell(ShellSize { cols: 80, rows: 24 })
                .err(),
            Some(ShellError::CannotDropPrivileges)
        );
        assert!(matches!(Person::Nobody.disk(), DiskAs::Nobody));
        assert!(matches!(Person::default().disk(), DiskAs::ThisProcess));
    }
}
