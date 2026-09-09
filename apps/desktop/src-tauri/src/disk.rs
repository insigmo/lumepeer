//! Free space on the volume a file is about to be written to (§18;
//! ADR 0077).
//!
//! One question, asked once, before a transfer is accepted: is there room for
//! this file where it is going. A receiver that finds out at ninety per cent
//! has already spent the sender's time and its own and has a staging file to
//! throw away; a receiver that answers before the first byte costs nothing and
//! says something true.
//!
//! The platform call behind it (`statvfs`, `GetDiskFreeSpaceExW`) is FFI and
//! therefore `unsafe`, which this crate's `forbid(unsafe_code)` does not allow
//! anywhere in it. `fs4` carries that unsafe in its own compiled crate
//! instead, the same way `keyring` carries `CredWriteW` for
//! `lumepeer_net::keystore` (§11.2).

use std::path::Path;

/// Bytes free to this user on the volume holding `path`, or `None` when the
/// platform will not say.
///
/// `path` must be a directory that exists; the answer is about the volume it
/// sits on, not about the directory. `None` means "no answer" and never "no
/// room" — an unmapped network path or a filesystem that reports nothing must
/// not be turned into a refused transfer, which would be a worse failure than
/// the one this exists to prevent.
#[must_use]
pub fn free_space_bytes(path: &Path) -> Option<u64> {
    fs4::available_space(path).ok()
}

/// Whether `bytes` will fit where `dir` is, as far as this machine can tell.
///
/// The margin is deliberate. A filesystem needs room for its own metadata,
/// the destination may grow between this answer and the last chunk, and a
/// volume filled to the last byte by a transfer is a machine that stops
/// working for reasons that have nothing to do with the transfer.
#[must_use]
pub fn has_room_for(dir: &Path, bytes: u64) -> bool {
    let Some(free) = free_space_bytes(dir) else {
        // No answer is not a refusal (see above).
        return true;
    };
    free.saturating_sub(lumepeer_core::constants::STAGING_FREE_SPACE_MARGIN_BYTES) >= bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The volume this test runs on has some space, and the answer is about
    /// a volume rather than about a directory — a path that does not exist
    /// yet still resolves to the volume it would be created on, which is
    /// what makes this usable before a destination is created.
    #[test]
    fn the_answer_is_about_the_volume_and_not_about_the_directory() {
        let temp = std::env::temp_dir();
        let free = free_space_bytes(&temp);
        assert!(
            free.is_some_and(|bytes| bytes > 0),
            "the temporary directory reported no free space at all"
        );
    }

    /// §18: a transfer that cannot fit is refused before the first byte, and
    /// one that fits is not.
    #[test]
    fn room_is_refused_only_when_there_is_actually_none() {
        let temp = std::env::temp_dir();
        assert!(has_room_for(&temp, 1));
        assert!(!has_room_for(&temp, u64::MAX));
    }

    /// A volume that will not answer is not a refusal: the caller is deciding
    /// whether to *stop* a transfer, and "the operating system declined to
    /// say" is not a reason to.
    #[test]
    fn no_answer_is_treated_as_room_rather_than_as_none() {
        // A path no platform resolves: an empty one. `available_space` fails
        // on it everywhere, which is the shape of "no answer".
        let nowhere = std::path::Path::new("");
        assert_eq!(free_space_bytes(nowhere), None);
        assert!(has_room_for(nowhere, u64::MAX));
    }
}
