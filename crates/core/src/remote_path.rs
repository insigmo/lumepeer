//! Paths that arrived over the wire (design doc §9.1, §9.2, §18; ADR 0075).
//!
//! [`crate::protocol::MessageKind::DirListRequest`] carries a path a guest
//! chose, which is to say a path an attacker chose. `safe_file_name` in
//! `lumepeer_net::file_transfer` already does this job for a single *name*;
//! this does it for a whole path, in the same spirit and with the same
//! answer to a bad one — refuse it, never rewrite it into something the host
//! did not mean.
//!
//! Deliberately its own string parser rather than `std::path`. The host and
//! the guest need not be the same operating system, so a Linux host has to
//! reject `..\\..\\windows` and a Windows host has to reject `../../etc`,
//! and `std::path::Component` only understands the separators of the platform
//! it was compiled for. Everything here treats `/` and `\` as separators on
//! every platform, which is the only reading that is safe on all of them.

use crate::constants::{DIR_PATH_MAX_BYTES, FILE_NAME_MAX_BYTES};

/// Windows device names, which address a device rather than a file no matter
/// which directory they appear in.
///
/// Checked on every platform for the same reason the separators are: the peer
/// that sent the path may be a different operating system than the one
/// reading it, and a name that means a device on Windows is refused rather
/// than served from a Linux host and then re-offered to a Windows guest.
pub const WINDOWS_DEVICE_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Whether `component` is a single ordinary path component: not a traversal,
/// not a device, not a name that becomes a different name when written.
///
/// The same set of refusals `safe_file_name` applies to an offered file name,
/// which is what keeps one hostile string from being refused in a transfer
/// and accepted in a listing.
#[must_use]
pub fn is_safe_component(component: &str) -> bool {
    if component.is_empty() || component.len() > FILE_NAME_MAX_BYTES {
        return false;
    }
    // `.` and `..` are the traversal itself; every other name is judged on
    // its characters.
    if component == "." || component == ".." {
        return false;
    }
    if component.chars().any(char::is_control) {
        return false;
    }
    // A colon is a drive letter or an NTFS alternate data stream, and neither
    // is a component of a directory path.
    if component.contains(':') {
        return false;
    }
    // Windows silently strips trailing dots and spaces, so a name ending in
    // one is a name that becomes a *different* name once it reaches the disk.
    if component.ends_with('.') || component.ends_with(' ') {
        return false;
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .to_ascii_uppercase();
    !WINDOWS_DEVICE_NAMES.contains(&stem.as_str())
}

/// Accepts an absolute path a peer asked to list, or refuses it (§9.1, §18;
/// ADR 0075).
///
/// Accepted, and nothing else:
///
/// - a Unix absolute path, `/` or `/a/b`;
/// - a Windows drive-absolute path, `C:\` or `C:\a\b` (either separator).
///
/// Refused, whichever platform is reading:
///
/// - anything relative (`a/b`, `C:file`, a bare `\`), because the host would
///   have to decide what it was relative *to*;
/// - a UNC path (`\\server\share`) and the extended-length and device
///   namespaces (`\\?\`, `\\.\`), which reach machines and devices rather
///   than this host's directories;
/// - any `..` or `.` component, anywhere, including on a platform whose
///   `std::path` would not have recognized the separator;
/// - an empty component, so `C:\\a` and `/a//b` are refused rather than
///   quietly normalized;
/// - everything [`is_safe_component`] refuses, per component;
/// - anything over [`DIR_PATH_MAX_BYTES`].
///
/// Refused rather than sanitized, exactly as `safe_file_name` is: rewriting a
/// hostile path produces a listing of a directory neither side named.
#[must_use]
pub fn safe_browse_path(path: &str) -> Option<&str> {
    if path.is_empty() || path.len() > DIR_PATH_MAX_BYTES {
        return None;
    }
    if path.chars().any(char::is_control) {
        return None;
    }

    let rest = if let Some(rest) = path.strip_prefix('/') {
        // A Unix absolute path. `//server/share` is a UNC path in POSIX's own
        // implementation-defined form, so a second leading separator is
        // refused here rather than treated as an empty component below.
        if rest.starts_with('/') {
            return None;
        }
        rest
    } else {
        // A Windows drive-absolute path, and only that: `C:` and `C:file` are
        // drive-*relative*, which is a different directory per drive and per
        // process.
        let mut chars = path.chars();
        let drive = chars.next()?;
        if !drive.is_ascii_alphabetic() || chars.next()? != ':' {
            return None;
        }
        let after_drive = &path[2..];
        after_drive
            .strip_prefix('\\')
            .or_else(|| after_drive.strip_prefix('/'))?
    };

    // A single trailing separator is how a root is written (`/`, `C:\`) and
    // how people write directories generally; anything else empty between two
    // separators is not.
    let trimmed = rest.strip_suffix(['/', '\\']).unwrap_or(rest);
    if trimmed.is_empty() {
        return Some(path);
    }
    if trimmed.split(['/', '\\']).all(is_safe_component) {
        Some(path)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The traversal itself, in every spelling, refused on every platform —
    /// this is a unit test on purpose: it touches no filesystem, so it says
    /// the same thing on Windows, Linux and macOS (ADR 0075).
    #[test]
    fn traversal_is_refused_in_every_spelling_on_every_platform() {
        for path in [
            "/home/../etc/shadow",
            "/home/..",
            "/..",
            "/home/./secrets",
            "C:\\Users\\..\\Windows",
            "C:\\Users\\..",
            "C:/Users/../Windows",
            "C:\\Users\\.\\Public",
            // A Windows separator inside a Unix path and the other way
            // round: the host may not be the platform the guest is on.
            "/home/..\\etc",
            "C:\\Users\\../Windows",
        ] {
            assert!(
                safe_browse_path(path).is_none(),
                "{path} was accepted despite naming a traversal"
            );
        }
    }

    #[test]
    fn only_absolute_paths_are_accepted() {
        for path in [
            "/",
            "/home",
            "/home/beta/projects",
            "C:\\",
            "C:/",
            "C:\\Users\\beta",
            "C:/Users/beta",
            "/home/beta/",
        ] {
            assert_eq!(
                safe_browse_path(path),
                Some(path),
                "{path} is an ordinary absolute path"
            );
        }
        for path in [
            "",
            "home",
            "home/beta",
            "./home",
            "../home",
            "C:",
            "C:file",
            "C:Users\\beta",
            "\\Users",
            "1:\\Users",
        ] {
            assert!(
                safe_browse_path(path).is_none(),
                "{path} is not an absolute path and was accepted"
            );
        }
    }

    #[test]
    fn unc_and_the_device_namespaces_are_refused() {
        for path in [
            "\\\\server\\share",
            "\\\\server\\share\\dir",
            "\\\\?\\C:\\Users",
            "\\\\.\\PhysicalDrive0",
            "//server/share",
            "//?/C:/Users",
        ] {
            assert!(
                safe_browse_path(path).is_none(),
                "{path} reaches past this host and was accepted"
            );
        }
    }

    #[test]
    fn device_names_and_streams_are_refused_anywhere_in_the_path() {
        for path in [
            "/home/NUL",
            "/home/nul/deeper",
            "C:\\Users\\CON",
            "C:\\Users\\com1\\logs",
            "C:\\Users\\LPT9.txt",
            // An alternate data stream is a colon after the drive.
            "C:\\Users\\notes.txt:hidden",
            "/home/notes:stream",
        ] {
            assert!(
                safe_browse_path(path).is_none(),
                "{path} names a device or a stream and was accepted"
            );
        }
    }

    #[test]
    fn empty_components_and_trailing_punctuation_are_refused() {
        for path in [
            "/home//beta",
            "C:\\Users\\\\beta",
            "C:\\Users\\beta.",
            "C:\\Users\\beta ",
            "/home/beta.",
            "/home/beta /projects",
        ] {
            assert!(
                safe_browse_path(path).is_none(),
                "{path} does not survive a round trip through a filesystem and was accepted"
            );
        }
    }

    #[test]
    fn control_characters_and_overlong_paths_are_refused() {
        assert!(safe_browse_path("/home/beta\u{0}/x").is_none());
        assert!(safe_browse_path("/home/be\nta").is_none());
        let long_component = "a".repeat(FILE_NAME_MAX_BYTES + 1);
        assert!(safe_browse_path(&format!("/home/{long_component}")).is_none());
        let deep = format!("/{}", "a/".repeat(DIR_PATH_MAX_BYTES));
        assert!(safe_browse_path(&deep).is_none());
    }

    #[test]
    fn a_component_is_judged_the_same_way_a_file_name_is() {
        assert!(is_safe_component("notes.txt"));
        assert!(is_safe_component("Project X"));
        assert!(!is_safe_component(""));
        assert!(!is_safe_component("."));
        assert!(!is_safe_component(".."));
        assert!(!is_safe_component("CON"));
        assert!(!is_safe_component("con.txt"));
        assert!(!is_safe_component("notes.txt:stream"));
        assert!(!is_safe_component("trailing."));
        assert!(!is_safe_component("trailing "));
    }
}
