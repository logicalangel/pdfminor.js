//! Python `str` for extracted text.
//!
//! A Python `str` is a sequence of code points 0..=0x10FFFF, *including* lone surrogates
//! (0xD800..=0xDFFF), which pdfminer produces with `chr(cid)` (Identity unicode maps) and keeps in
//! the extracted text. Rust's `String` cannot hold them, so text that comes from fonts and unicode
//! maps is a [`PyStr`]: the code points stored as generalized UTF-8 (a surrogate is encoded like
//! any other 3-byte code point, and a high + low pair stays two code points, as in Python).
//! Without surrogates the bytes are plain UTF-8 and [`PyStr::as_str`] is free.
//!
//! JavaScript strings are UTF-16 and hold lone surrogates too, so [`PyStr::to_utf16`] is lossless.

use std::fmt;

use super::ctype::py_isspace;
use super::error::{ExcKind, PyErr, PyResult};

#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PyStr {
    /// Generalized UTF-8.
    bytes: Vec<u8>,
}

/// Encodes one code point (surrogates included) as generalized UTF-8.
fn push_cp(out: &mut Vec<u8>, cp: u32) {
    debug_assert!(cp <= 0x10FFFF);
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x800 {
        out.extend_from_slice(&[0xC0 | (cp >> 6) as u8, 0x80 | (cp & 0x3F) as u8]);
    } else if cp < 0x10000 {
        out.extend_from_slice(&[0xE0 | (cp >> 12) as u8, 0x80 | ((cp >> 6) & 0x3F) as u8, 0x80 | (cp & 0x3F) as u8]);
    } else {
        out.extend_from_slice(&[
            0xF0 | (cp >> 18) as u8,
            0x80 | ((cp >> 12) & 0x3F) as u8,
            0x80 | ((cp >> 6) & 0x3F) as u8,
            0x80 | (cp & 0x3F) as u8,
        ]);
    }
}

/// Iterator over the code points of a [`PyStr`].
#[derive(Clone)]
pub struct CodePoints<'a> {
    bytes: &'a [u8],
    i: usize,
}

impl Iterator for CodePoints<'_> {
    type Item = u32;
    #[inline]
    fn next(&mut self) -> Option<u32> {
        let b = self.bytes;
        let i = self.i;
        if i >= b.len() {
            return None;
        }
        let b0 = b[i] as u32;
        let (cp, n) = if b0 < 0x80 {
            (b0, 1)
        } else if b0 < 0xE0 {
            (((b0 & 0x1F) << 6) | (b[i + 1] as u32 & 0x3F), 2)
        } else if b0 < 0xF0 {
            (((b0 & 0x0F) << 12) | ((b[i + 1] as u32 & 0x3F) << 6) | (b[i + 2] as u32 & 0x3F), 3)
        } else {
            (
                ((b0 & 0x07) << 18)
                    | ((b[i + 1] as u32 & 0x3F) << 12)
                    | ((b[i + 2] as u32 & 0x3F) << 6)
                    | (b[i + 3] as u32 & 0x3F),
                4,
            )
        };
        self.i += n;
        Some(cp)
    }
}

impl PyStr {
    pub fn new() -> Self {
        PyStr { bytes: Vec::new() }
    }

    /// `chr(cp)`: `ValueError` above 0x10FFFF (and for negatives), like Python.
    pub fn chr(cp: i64) -> PyResult<PyStr> {
        if !(0..=0x10FFFF).contains(&cp) {
            return Err(PyErr::new(ExcKind::ValueError, "chr() arg not in range(0x110000)"));
        }
        let mut s = PyStr::new();
        push_cp(&mut s.bytes, cp as u32);
        Ok(s)
    }

    pub fn from_code_points(cps: impl IntoIterator<Item = u32>) -> PyStr {
        let mut s = PyStr::new();
        for cp in cps {
            push_cp(&mut s.bytes, cp);
        }
        s
    }

    /// `bytes.decode("utf-16be", "ignore")`: lone surrogates are dropped (Python's UTF-16 codec
    /// never yields them), a trailing odd byte is dropped.
    pub fn decode_utf16be_ignore(data: &[u8]) -> PyStr {
        let units = data.as_chunks::<2>().0.iter().map(|&c| u16::from_be_bytes(c));
        let mut s = PyStr::new();
        for c in char::decode_utf16(units).flatten() {
            s.push_char(c);
        }
        s
    }

    pub fn push_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }

    pub fn push_cp(&mut self, cp: u32) {
        push_cp(&mut self.bytes, cp);
    }

    pub fn push_str(&mut self, s: &str) {
        self.bytes.extend_from_slice(s.as_bytes());
    }

    pub fn push_pystr(&mut self, s: &PyStr) {
        self.bytes.extend_from_slice(&s.bytes);
    }

    pub fn code_points(&self) -> CodePoints<'_> {
        CodePoints { bytes: &self.bytes, i: 0 }
    }

    /// `len(s)`: the number of code points.
    pub fn len(&self) -> usize {
        // Count the bytes that start a code point.
        self.bytes.iter().filter(|&&b| (b & 0xC0) != 0x80).count()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The text as `&str` when it has no surrogates.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }

    /// `s.encode("utf-8")` (strict): `UnicodeEncodeError` on a surrogate.
    pub fn encode_utf8(&self) -> PyResult<&str> {
        self.as_str().ok_or_else(|| {
            PyErr::new(ExcKind::UnicodeEncodeError, "'utf-8' codec can't encode character: surrogates not allowed")
        })
    }

    /// The text with every surrogate replaced by U+FFFD (for logs and lossy outputs only).
    pub fn to_string_lossy(&self) -> String {
        self.code_points().map(|cp| char::from_u32(cp).unwrap_or('\u{FFFD}')).collect()
    }

    /// UTF-16 code units, as a JavaScript string holds them (lossless).
    pub fn to_utf16(&self) -> Vec<u16> {
        let mut out = Vec::with_capacity(self.bytes.len());
        for cp in self.code_points() {
            if cp >= 0x10000 {
                let v = cp - 0x10000;
                out.push(0xD800 | (v >> 10) as u16);
                out.push(0xDC00 | (v & 0x3FF) as u16);
            } else {
                out.push(cp as u16);
            }
        }
        out
    }

    /// `s.isspace()`: true when non-empty and every code point is Python whitespace.
    pub fn isspace(&self) -> bool {
        !self.is_empty() && self.code_points().all(py_isspace)
    }

    /// `s.strip()` (no argument): strips Python whitespace from both ends.
    pub fn strip(&self) -> PyStr {
        let cps: Vec<u32> = self.code_points().collect();
        let start = cps.iter().position(|&c| !py_isspace(c)).unwrap_or(cps.len());
        let end = cps.iter().rposition(|&c| !py_isspace(c)).map_or(start, |e| e + 1);
        PyStr::from_code_points(cps[start..end].iter().copied())
    }

    /// `json.dumps(s)` with the default `ensure_ascii=True`, quotes included.
    pub fn json_ascii(&self, out: &mut String) {
        out.push('"');
        for cp in self.code_points() {
            json_escape_cp(cp, out);
        }
        out.push('"');
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// One code point of a JSON string as `json.dumps(..., ensure_ascii=True)` writes it.
pub fn json_escape_cp(cp: u32, out: &mut String) {
    use std::fmt::Write;
    match cp {
        0x22 => out.push_str("\\\""),
        0x5C => out.push_str("\\\\"),
        0x0A => out.push_str("\\n"),
        0x0D => out.push_str("\\r"),
        0x09 => out.push_str("\\t"),
        0x08 => out.push_str("\\b"),
        0x0C => out.push_str("\\f"),
        0x20..=0x7E => out.push(cp as u8 as char),
        0x10000.. => {
            let v = cp - 0x10000;
            let _ = write!(out, "\\u{:04x}\\u{:04x}", 0xD800 | (v >> 10), 0xDC00 | (v & 0x3FF));
        }
        _ => {
            let _ = write!(out, "\\u{:04x}", cp);
        }
    }
}

impl From<&str> for PyStr {
    fn from(s: &str) -> Self {
        PyStr { bytes: s.as_bytes().to_vec() }
    }
}

impl From<String> for PyStr {
    fn from(s: String) -> Self {
        PyStr { bytes: s.into_bytes() }
    }
}

impl From<char> for PyStr {
    fn from(c: char) -> Self {
        let mut s = PyStr::new();
        s.push_char(c);
        s
    }
}

impl fmt::Debug for PyStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.json_ascii(&mut s);
        f.write_str(&s)
    }
}

impl fmt::Display for PyStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surrogates_round_trip() {
        let mut s = PyStr::chr(0xD83D).unwrap();
        s.push_pystr(&PyStr::chr(0xDE00).unwrap());
        assert_eq!(s.len(), 2);
        assert_eq!(s.to_utf16(), vec![0xD83D, 0xDE00]);
        assert!(s.as_str().is_none());
        let mut j = String::new();
        s.json_ascii(&mut j);
        assert_eq!(j, "\"\\ud83d\\ude00\"");
        let e = PyStr::from("😀");
        assert_eq!(e.len(), 1);
        let mut j2 = String::new();
        e.json_ascii(&mut j2);
        assert_eq!(j2, j);
    }

    #[test]
    fn utf16be_ignore_drops_lone_surrogates() {
        let s = PyStr::decode_utf16be_ignore(&[0xD8, 0x00, 0x00, 0x41, 0x00]);
        assert_eq!(s.as_str(), Some("A"));
    }

    #[test]
    fn strip_and_isspace() {
        let s = PyStr::from("\u{1c} a\u{a0}\u{3000}");
        assert_eq!(s.strip().as_str(), Some("a"));
        assert!(PyStr::from("\u{85}\u{2028}").isspace());
        assert!(!PyStr::from("").isspace());
        assert!(!PyStr::from("\u{200b}").isspace());
    }
}
