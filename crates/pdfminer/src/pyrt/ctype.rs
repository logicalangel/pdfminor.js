//! Python's character classes, which differ from Rust's.
//!
//! `str.isspace()` is "bidirectional class WS, B or S, or category Zs", not Unicode `White_Space`
//! (it includes U+001C..U+001F and excludes U+180E, for example). `bytes` methods only know ASCII.
//! The tables come from the reference Python (see `tools/codegen.py`).

use super::unicode_tables::{NONPRINTABLE_RANGES, SPACE_CODE_POINTS};

/// `chr(cp).isspace()`.
#[inline]
pub fn py_isspace(cp: u32) -> bool {
    if cp < 0x80 {
        matches!(cp, 0x09..=0x0D | 0x1C..=0x20)
    } else {
        SPACE_CODE_POINTS.binary_search(&cp).is_ok()
    }
}

/// `chr(cp).isprintable()` (used by `repr` of a `str`).
pub fn py_isprintable(cp: u32) -> bool {
    NONPRINTABLE_RANGES
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_err()
}

/// `bytes.isspace()` per byte, and the `\s` class of a `bytes` regex: space, `\t\n\r\v\f`.
#[inline]
pub fn byte_isspace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C)
}

/// `bytes.isdigit()` per byte.
#[inline]
pub fn byte_isdigit(b: u8) -> bool {
    b.is_ascii_digit()
}

/// `bytes.isalpha()` per byte.
#[inline]
pub fn byte_isalpha(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// `b.isdigit()` for a whole `bytes`: non-empty and all ASCII digits.
pub fn bytes_isdigit(s: &[u8]) -> bool {
    !s.is_empty() && s.iter().all(|b| b.is_ascii_digit())
}

/// `b.strip()` for `bytes` (ASCII whitespace).
pub fn bytes_strip(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&b| !byte_isspace(b)).unwrap_or(s.len());
    let end = s.iter().rposition(|&b| !byte_isspace(b)).map_or(start, |e| e + 1);
    &s[start..end]
}

/// `s.strip()` for a Rust `&str` holding Python text (Python whitespace).
pub fn str_strip(s: &str) -> &str {
    s.trim_matches(|c: char| py_isspace(c as u32))
}

/// `s.isspace()` for a Rust `&str`.
pub fn str_isspace(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| py_isspace(c as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_whitespace() {
        for cp in [9, 10, 11, 12, 13, 28, 29, 30, 31, 32, 133, 160, 5760, 8192, 8202, 8232, 8233, 8239, 8287, 12288] {
            assert!(py_isspace(cp), "{cp}");
        }
        for cp in [0, 8, 33, 0x180E, 0x200B, 0xFEFF] {
            assert!(!py_isspace(cp), "{cp}");
        }
    }

    #[test]
    fn printable() {
        assert!(py_isprintable('a' as u32));
        assert!(py_isprintable(' ' as u32));
        assert!(!py_isprintable('\n' as u32));
        assert!(!py_isprintable(0xD800));
        assert!(!py_isprintable(0xA0));
    }
}
