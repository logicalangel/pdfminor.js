//! The `bytes.decode(...)` / `str.encode(...)` calls pdfminer makes, with Python's error
//! handlers.

use super::pystr::PyStr;

/// `b.decode("utf-8", "ignore")`: invalid sequences are dropped.
pub fn utf8_decode_ignore(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for chunk in b.utf8_chunks() {
        out.push_str(chunk.valid());
    }
    out
}

/// `b.decode("utf-8")` (strict); `None` where Python raises `UnicodeDecodeError`.
pub fn utf8_decode_strict(b: &[u8]) -> Option<&str> {
    std::str::from_utf8(b).ok()
}

/// `b.decode("latin-1")`.
pub fn latin1_decode(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// `b.decode("utf-16be", "ignore")`.
pub fn utf16be_decode_ignore(b: &[u8]) -> PyStr {
    PyStr::decode_utf16be_ignore(b)
}
