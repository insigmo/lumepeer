//! What a peer says its machine is called (ADR 0121).
//!
//! `MessageKind::DeviceInfo` is the one place a peer puts text of its own
//! choosing into this machine's session list, so it is cleaned here, once,
//! before anything stores it. `check_limits` has already bounded the bytes;
//! what is left is making sure the text shows as what it is — a name — and
//! cannot rearrange or hide the rest of the row it sits in.

use crate::constants::DEVICE_NAME_MAX_CHARS;

/// Longest operating-system tag kept (ADR 0121). Every value
/// `std::env::consts::OS` has fits.
const OS_TAG_MAX_CHARS: usize = 16;

/// A peer's machine name made fit to show, or `None` when nothing showable is
/// left.
///
/// Control characters, and the invisible formatting characters that reorder
/// or hide text (bidi overrides and isolates, zero-width joiners), are
/// dropped: a name must not be able to turn the label beside it around. Runs
/// of whitespace become one space, the ends are trimmed, and what remains is
/// cut to [`DEVICE_NAME_MAX_CHARS`].
#[must_use]
pub fn clean_name(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut shown = 0usize;
    let mut pending_space = false;
    for c in raw.chars() {
        // Whitespace first: a line break or a tab is a control character too,
        // and it separates words rather than vanishing between them.
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if c.is_control() || is_invisible_format(c) {
            continue;
        }
        if pending_space {
            if shown + 1 >= DEVICE_NAME_MAX_CHARS {
                break;
            }
            out.push(' ');
            shown += 1;
            pending_space = false;
        }
        if shown == DEVICE_NAME_MAX_CHARS {
            break;
        }
        out.push(c);
        shown += 1;
    }
    (!out.is_empty()).then_some(out)
}

/// A peer's operating-system tag reduced to what a tag can be — lowercase
/// ASCII letters, digits, `_` and `-` — or `None` when nothing is left.
///
/// Whatever survives is only ever compared against the handful of values the
/// interface draws an icon for; anything else gets the generic one.
#[must_use]
pub fn clean_os(raw: &str) -> Option<String> {
    let tag: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .map(|c| c.to_ascii_lowercase())
        .take(OS_TAG_MAX_CHARS)
        .collect();
    (!tag.is_empty()).then_some(tag)
}

/// Characters that change how the text around them is laid out, or occupy no
/// room at all, without being control characters in Unicode's own sense.
const fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn an_ordinary_name_is_kept_as_it_is() {
        assert_eq!(clean_name("BETA-PC").as_deref(), Some("BETA-PC"));
        assert_eq!(
            clean_name("MacBook Pro (Беты)").as_deref(),
            Some("MacBook Pro (Беты)")
        );
    }

    #[test]
    fn a_name_cannot_turn_the_row_around_or_hide_in_it() {
        // U+202E would render everything after it right to left.
        assert_eq!(
            clean_name("abc\u{202E}gpj.exe").as_deref(),
            Some("abcgpj.exe")
        );
        assert_eq!(
            clean_name("a\u{200B}b\u{2066}c\u{FEFF}").as_deref(),
            Some("abc")
        );
        assert_eq!(
            clean_name("line\none\tand\r\u{7}two").as_deref(),
            Some("line one and two")
        );
    }

    #[test]
    fn whitespace_collapses_and_the_ends_are_trimmed() {
        assert_eq!(
            clean_name("   my    laptop  ").as_deref(),
            Some("my laptop")
        );
    }

    #[test]
    fn nothing_showable_is_no_name() {
        assert_eq!(clean_name(""), None);
        assert_eq!(clean_name(" \t\u{202E}\u{200B} "), None);
    }

    #[test]
    fn a_long_name_is_cut_to_the_bound_in_characters() {
        let name = clean_name(&"я".repeat(DEVICE_NAME_MAX_CHARS * 2)).unwrap();
        assert_eq!(name.chars().count(), DEVICE_NAME_MAX_CHARS);
        let spaced = clean_name(&"ab ".repeat(DEVICE_NAME_MAX_CHARS)).unwrap();
        assert!(spaced.chars().count() <= DEVICE_NAME_MAX_CHARS);
        assert!(!spaced.ends_with(' '));
    }

    #[test]
    fn an_os_tag_is_lowercase_ascii_or_nothing() {
        assert_eq!(clean_os("windows").as_deref(), Some("windows"));
        assert_eq!(clean_os("MacOS").as_deref(), Some("macos"));
        assert_eq!(clean_os("<svg onload=x>").as_deref(), Some("svgonloadx"));
        assert_eq!(clean_os("ОС"), None);
        assert_eq!(clean_os(""), None);
    }
}
