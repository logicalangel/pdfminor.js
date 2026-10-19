//! Port of `pdfminer/ascii85.py`.
//!
//! Python implementation of ASCII85/ASCIIHex decoder (Adobe version).
//!
//! The regexes are byte scans with the same matches, and `base64.a85decode` /
//! `binascii.unhexlify` are ports of CPython 3.14's (`Lib/base64.py`, `Modules/binascii.c`),
//! with the same `ValueError`/`binascii.Error` at the same input.

use crate::pyrt::error::{ExcKind, PyErr, PyResult};

/// `\s` in a bytes regex: `[ \t\n\r\f\v]`.
fn is_re_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `start_re = re.compile(rb"^\s*<?\s*~\s*")`: the length of the match at the start of `data`,
/// if any. (Greedy and deterministic: an optional `<` can only be followed by `\s*~`.)
fn start_re_match(data: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < data.len() && is_re_space(data[i]) {
        i += 1;
    }
    if i < data.len() && data[i] == b'<' {
        i += 1;
    }
    while i < data.len() && is_re_space(data[i]) {
        i += 1;
    }
    if i < data.len() && data[i] == b'~' {
        i += 1;
    } else {
        return None;
    }
    while i < data.len() && is_re_space(data[i]) {
        i += 1;
    }
    Some(i)
}

/// `end_re = re.compile(rb"\s*~\s*>?\s*$")`: the start of the leftmost match, if any. Every
/// match runs to the end of `data` (`\s*` takes a final newline before `$` can match before
/// it), so the match is the last `~`, optionally followed by `>`, with only whitespace around,
/// plus all whitespace before that `~`.
fn end_re_match(data: &[u8]) -> Option<usize> {
    let mut j = data.len();
    while j > 0 && is_re_space(data[j - 1]) {
        j -= 1;
    }
    if j > 0 && data[j - 1] == b'>' {
        j -= 1;
        while j > 0 && is_re_space(data[j - 1]) {
            j -= 1;
        }
    }
    if j > 0 && data[j - 1] == b'~' {
        j -= 1;
    } else {
        return None;
    }
    while j > 0 && is_re_space(data[j - 1]) {
        j -= 1;
    }
    Some(j)
}

/// In ASCII85 encoding, every four bytes are encoded with five ASCII
/// letters, using 85 different types of characters (as 256**4 < 85**5).
/// When the length of the original bytes is not a multiple of 4, a special
/// rule is used for round up.
///
/// Adobe's ASCII85 implementation expects the input to be terminated
/// by `b"~>"`, and (though this is absent from the PDF spec) it can
/// also begin with `b"<~"`.  We can't reliably expect this to be the
/// case, and there can be off-by-one errors in stream lengths which
/// mean we only see `~` at the end.  Worse yet, `<` and `>` are
/// ASCII85 digits, so we can't strip them.  We settle on a compromise
/// where we strip leading `<~` or `~` and trailing `~` or `~>`.
pub fn ascii85decode(data: &[u8]) -> PyResult<Vec<u8>> {
    let mut data = data;
    if let Some(n) = start_re_match(data) {
        data = &data[n..];
    }
    if let Some(n) = end_re_match(data) {
        data = &data[..n];
    }
    a85decode(data)
}

/// `base64.a85decode(b)` with the defaults (`foldspaces=False`, `adobe=False`,
/// `ignorechars=b' \t\n\r\v'`).
pub fn a85decode(b: &[u8]) -> PyResult<Vec<u8>> {
    const IGNORECHARS: &[u8] = b" \t\n\r\x0b";
    let mut decoded: Vec<u8> = Vec::with_capacity(b.len() / 5 * 4 + 4);
    let mut curr: [u8; 5] = [0; 5];
    let mut ncurr = 0usize;
    for &x in b.iter().chain(b"uuuu".iter()) {
        if (b'!'..=b'u').contains(&x) {
            curr[ncurr] = x;
            ncurr += 1;
            if ncurr == 5 {
                let mut acc: u64 = 0;
                for &c in &curr {
                    acc = 85 * acc + (c - 33) as u64;
                }
                if acc > u32::MAX as u64 {
                    return Err(PyErr::new(ExcKind::ValueError, "Ascii85 overflow"));
                }
                decoded.extend_from_slice(&(acc as u32).to_be_bytes());
                ncurr = 0;
            }
        } else if x == b'z' {
            if ncurr != 0 {
                return Err(PyErr::new(ExcKind::ValueError, "z inside Ascii85 5-tuple"));
            }
            decoded.extend_from_slice(b"\0\0\0\0");
        } else if IGNORECHARS.contains(&x) {
            // Skip whitespace
            continue;
        } else {
            // '%c' % x: the code point x (a latin-1 character for x >= 128)
            return Err(PyErr::new(ExcKind::ValueError, format!("Non-Ascii85 digit found: {}", char::from(x))));
        }
    }

    let padding = 4 - ncurr;
    if padding != 0 {
        // Throw away the extra padding
        decoded.truncate(decoded.len().saturating_sub(padding));
    }
    Ok(decoded)
}

/// `bws_re = re.compile(rb"\s")`
fn bws_re_sub(data: &[u8]) -> Vec<u8> {
    data.iter().copied().filter(|&c| !is_re_space(c)).collect()
}

/// ASCIIHexDecode filter: PDFReference v1.4 section 3.3.1
/// For each pair of ASCII hexadecimal digits (0-9 and A-F or a-f), the
/// ASCIIHexDecode filter produces one byte of binary data. All white-space
/// characters are ignored. A right angle bracket character (>) indicates
/// EOD. Any other characters will cause an error. If the filter encounters
/// the EOD marker after reading an odd number of hexadecimal digits, it
/// will behave as if a 0 followed the last digit.
pub fn asciihexdecode(data: &[u8]) -> PyResult<Vec<u8>> {
    let mut data = bws_re_sub(data);
    if let Some(idx) = data.iter().position(|&c| c == b'>') {
        data.truncate(idx);
        if idx % 2 == 1 {
            data.push(b'0');
        }
    }
    unhexlify(&data)
}

/// `binascii.unhexlify(data)`.
pub fn unhexlify(data: &[u8]) -> PyResult<Vec<u8>> {
    if !data.len().is_multiple_of(2) {
        return Err(PyErr::new(ExcKind::BinasciiError, "Odd-length string"));
    }
    fn digit(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(data.len() / 2);
    for &[hi, lo] in data.as_chunks::<2>().0 {
        match (digit(hi), digit(lo)) {
            (Some(top), Some(bot)) => out.push((top << 4) + bot),
            _ => return Err(PyErr::new(ExcKind::BinasciiError, "Non-hexadecimal digit found")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // tests/test_pdfminer_crypto.py: TestAscii85

    #[test]
    fn test_ascii85decode() {
        // The sample string is taken from:
        // http://en.wikipedia.org/w/index.php?title=Ascii85
        assert_eq!(ascii85decode(b"9jqo^BlbD-BleB1DJ+*+F(f,q").unwrap(), b"Man is distinguished");
        assert_eq!(ascii85decode(b"E,9)oF*2M7/c~>").unwrap(), b"pleasure.");
        assert_eq!(ascii85decode(b"zE,9)oF*2M7/c~>").unwrap(), b"\0\0\0\0pleasure.");
        // And some bogus cases you may encounter
        assert_eq!(ascii85decode(b"E,9)oF*2M7/c").unwrap(), b"pleasure.");
        assert_eq!(ascii85decode(b"E,9)oF*2M7/c~").unwrap(), b"pleasure.");
        assert_eq!(ascii85decode(b"<~E,9)oF*2M7/c~").unwrap(), b"pleasure.");
        assert_eq!(ascii85decode(b"<~E,9)oF*2M7/c~\n>").unwrap(), b"pleasure.");
        // Ensure that we don't miss actual ASCII85 digits
        assert_eq!(ascii85decode(b"<^BVT:K:=9<E)pd;BS_1:/aSV;ag~>").unwrap(), b"VARIOUS UTTER NONSENSE");
        assert_eq!(ascii85decode(b"<~<^BVT:K:=9<E)pd;BS_1:/aSV;ag~>").unwrap(), b"VARIOUS UTTER NONSENSE");
        assert_eq!(ascii85decode(b"<^BVT:K:=9<E)pd;BS_1:/aSV;ag~").unwrap(), b"VARIOUS UTTER NONSENSE");
    }

    #[test]
    fn test_asciihexdecode() {
        assert_eq!(asciihexdecode(b"61 62 2e6364   65").unwrap(), b"ab.cde");
        assert_eq!(asciihexdecode(b"61 62 2e6364   657>").unwrap(), b"ab.cdep");
        assert_eq!(asciihexdecode(b"7>").unwrap(), b"p");
    }

    #[test]
    fn errors() {
        assert!(ascii85decode(b"ab\x0ccd").unwrap_err().is(ExcKind::ValueError));
        assert!(ascii85decode(b"abzcd").unwrap_err().is(ExcKind::ValueError));
        assert_eq!(ascii85decode(b"uuuuu").unwrap_err().msg, "Ascii85 overflow");
        assert!(asciihexdecode(b"6").unwrap_err().is(ExcKind::BinasciiError));
        assert!(asciihexdecode(b"6g").unwrap_err().is(ExcKind::ValueError));
    }
}
