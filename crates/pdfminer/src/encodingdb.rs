//! Port of `pdfminer/encodingdb.py`.

use std::collections::HashMap;
use std::rc::Rc;

use log::debug;

use crate::glyphlist::glyphname2unicode;
use crate::latin_enc::ENCODING;
use crate::psparser::NameType;
use crate::pyrt::ctype::py_isspace;
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::pystr::PyStr;
use crate::pyrt::value::Value;

/// `HEXADECIMAL = re.compile(r"[0-9a-fA-F]+")`: `HEXADECIMAL.match(s)` succeeds when the first
/// character is an ASCII hex digit.
fn hexadecimal_match(s: &[char]) -> bool {
    s.first().is_some_and(|c| c.is_ascii_hexdigit())
}

/// The code points whose decimal value is 0, for every run of Unicode decimal digits (category
/// Nd) above ASCII; each run holds the digits 0..9. `int()` accepts these digits.
/// From Python 3.14.7: `[c for c in range(0x80, 0x110000) if unicodedata.decimal(chr(c), -1) == 0]`
/// (every one starts a run of ten).
const DECIMAL_ZEROS: &[u32] = &[
    0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66, 0xDE6, 0xE50, 0xED0, 0xF20,
    0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0,
    0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x10D40, 0x11066, 0x110F0, 0x11136, 0x111D0,
    0x112F0, 0x11450, 0x114D0, 0x11650, 0x116C0, 0x116D0, 0x116DA, 0x11730, 0x118E0, 0x11950, 0x11BF0, 0x11C50,
    0x11D50, 0x11DA0, 0x11F50, 0x16130, 0x16A60, 0x16AC0, 0x16B50, 0x16D70, 0x1CCF0, 0x1D7CE, 0x1D7D8, 0x1D7E2,
    0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E5F1, 0x1E950, 0x1FBF0,
];

/// The decimal value of a non-ASCII Unicode digit.
fn unicode_decimal(cp: u32) -> Option<u32> {
    let i = DECIMAL_ZEROS.partition_point(|&z| z <= cp);
    if i == 0 {
        return None;
    }
    let z = DECIMAL_ZEROS[i - 1];
    if cp - z < 10 { Some(cp - z) } else { None }
}

/// `int(s, base=16)`: `None` where Python raises `ValueError`.
///
/// Python first maps Unicode whitespace to spaces and Unicode decimal digits to ASCII digits,
/// then strips whitespace and accepts a sign, an optional `0x` prefix and digits with single
/// underscores between them (one may follow the prefix).
pub(crate) fn py_int_base16(s: &[char]) -> Option<i64> {
    let t: Vec<char> = s
        .iter()
        .map(|&c| {
            let cp = c as u32;
            if cp < 0x80 {
                c
            } else if py_isspace(cp) {
                ' '
            } else if let Some(d) = unicode_decimal(cp) {
                char::from_digit(d, 10).unwrap()
            } else {
                '?'
            }
        })
        .collect();
    let mut i = 0;
    let mut j = t.len();
    while i < j && py_isspace(t[i] as u32) {
        i += 1;
    }
    while j > i && py_isspace(t[j - 1] as u32) {
        j -= 1;
    }
    let t = &t[i..j];
    let (neg, mut t) = match t.first() {
        Some('-') => (true, &t[1..]),
        Some('+') => (false, &t[1..]),
        _ => (false, t),
    };
    let mut after_prefix = false;
    if t.len() >= 2 && t[0] == '0' && (t[1] == 'x' || t[1] == 'X') {
        t = &t[2..];
        after_prefix = true;
    }
    if t.is_empty() {
        return None;
    }
    let mut v: i64 = 0;
    let mut prev_digit = after_prefix; // an underscore may directly follow the prefix
    for (k, &c) in t.iter().enumerate() {
        if c == '_' {
            if !prev_digit || k + 1 == t.len() {
                return None;
            }
            prev_digit = false;
            continue;
        }
        let d = c.to_digit(16)?;
        // At most 6 digits reach here (name2unicode slices them), so this cannot overflow.
        v = v.checked_mul(16)?.checked_add(d as i64)?;
        prev_digit = true;
    }
    Some(if neg { -v } else { v })
}

/// `str.strip(chars)`: strips any of `chars` from both ends.
fn strip_chars<'a>(s: &'a [char], chars: &[char]) -> &'a [char] {
    let mut i = 0;
    let mut j = s.len();
    while i < j && chars.contains(&s[i]) {
        i += 1;
    }
    while j > i && chars.contains(&s[j - 1]) {
        j -= 1;
    }
    &s[i..j]
}

/// The argument of `name2unicode`: Python accepts any object and raises for non-`str`.
pub enum NameArg<'a> {
    Str(&'a str),
    /// Anything that is not a `str`: its `type(name)` for the message.
    Other(&'static str, String),
}

impl<'a> From<&'a NameType> for NameArg<'a> {
    fn from(n: &'a NameType) -> Self {
        match n {
            NameType::Str(s) => NameArg::Str(s),
            NameType::Bytes(b) => NameArg::Other("bytes", crate::pyrt::fmt::bytes_repr(b)),
        }
    }
}

/// Converts Adobe glyph names to Unicode numbers.
///
/// In contrast to the specification, this raises a KeyError instead of return
/// an empty string when the key is unknown.
/// This way the caller must explicitly define what to do
/// when there is not a match.
///
/// Reference:
/// https://github.com/adobe-type-tools/agl-specification#2-the-mapping
///
/// :returns unicode character if name resembles something,
/// otherwise a KeyError
pub fn name2unicode(name: &str) -> PyResult<PyStr> {
    name2unicode_arg(NameArg::Str(name))
}

/// `name2unicode(name)` for any object.
pub fn name2unicode_arg(name: NameArg<'_>) -> PyResult<PyStr> {
    let name = match name {
        NameArg::Str(s) => s,
        NameArg::Other(type_name, value) => {
            return Err(PyErr::new(
                ExcKind::PDFKeyError,
                format!(
                    "'Could not convert unicode name \"{value}\" to character because it should be of type str \
                     but is of type <class \\'{type_name}\\'>'"
                ),
            ));
        }
    };

    let name = name.split('.').next().unwrap_or("");
    let components: Vec<&str> = name.split('_').collect();

    if components.len() > 1 {
        let mut out = PyStr::new();
        for c in components {
            out.push_pystr(&name2unicode(c)?);
        }
        return Ok(out);
    } else if let Some(u) = glyphname2unicode(name) {
        return Ok(PyStr::from(u));
    } else if name.starts_with("uni") {
        let chars: Vec<char> = name.chars().collect();
        let name_without_uni = strip_chars(&chars, &['u', 'n', 'i']);

        if hexadecimal_match(name_without_uni) && name_without_uni.len().is_multiple_of(4) {
            let mut unicode_digits = Vec::new();
            for chunk in name_without_uni.chunks(4) {
                match py_int_base16(chunk) {
                    Some(v) => unicode_digits.push(v),
                    None => {
                        return Err(PyErr::new(
                            ExcKind::ValueError,
                            format!(
                                "invalid literal for int() with base 16: {}",
                                crate::pyrt::fmt::str_repr(&chunk.iter().collect::<String>())
                            ),
                        ));
                    }
                }
            }
            for &digit in &unicode_digits {
                raise_key_error_for_invalid_unicode(digit)?;
            }
            let mut out = PyStr::new();
            for digit in unicode_digits {
                out.push_pystr(&PyStr::chr(digit)?);
            }
            return Ok(out);
        }
    } else if name.starts_with('u') {
        let chars: Vec<char> = name.chars().collect();
        let name_without_u = strip_chars(&chars, &['u']);

        if hexadecimal_match(name_without_u) && (4..=6).contains(&name_without_u.len()) {
            let Some(unicode_digit) = py_int_base16(name_without_u) else {
                return Err(PyErr::new(
                    ExcKind::ValueError,
                    format!(
                        "invalid literal for int() with base 16: {}",
                        crate::pyrt::fmt::str_repr(&name_without_u.iter().collect::<String>())
                    ),
                ));
            };
            raise_key_error_for_invalid_unicode(unicode_digit)?;
            return PyStr::chr(unicode_digit);
        }
    }

    Err(PyErr::new(
        ExcKind::PDFKeyError,
        format!("'Could not convert unicode name \"{name}\" to character because it does not match specification'"),
    ))
}

/// Unicode values should not be in the range D800 through DFFF because
/// that is used for surrogate pairs in UTF-16
///
/// :raises KeyError if unicode digit is invalid
pub fn raise_key_error_for_invalid_unicode(unicode_digit: i64) -> PyResult<()> {
    if 55295 < unicode_digit && unicode_digit < 57344 {
        return Err(PyErr::new(
            ExcKind::PDFKeyError,
            format!("'Unicode digit {unicode_digit} is invalid because it is in the range D800 through DFFF'"),
        ));
    }
    Ok(())
}

/// A `cid -> unicode` encoding dict, shared like Python shares the class-level dicts.
pub type Cid2Unicode = Rc<HashMap<i64, PyStr>>;

/// `EncodingDB`.
pub struct EncodingDB {
    pub std2unicode: Cid2Unicode,
    pub mac2unicode: Cid2Unicode,
    pub win2unicode: Cid2Unicode,
    pub pdf2unicode: Cid2Unicode,
}

thread_local! {
    static ENCODING_DB: EncodingDB = EncodingDB::build();
}

impl EncodingDB {
    fn build() -> EncodingDB {
        let mut std2unicode = HashMap::new();
        let mut mac2unicode = HashMap::new();
        let mut win2unicode = HashMap::new();
        let mut pdf2unicode = HashMap::new();
        for &(name, std, mac, win, pdf) in ENCODING {
            let c = name2unicode(name).expect("ENCODING names are valid glyph names");
            // `if std:` (0 is falsy)
            if let Some(std) = std.filter(|&v| v != 0) {
                std2unicode.insert(std as i64, c.clone());
            }
            if let Some(mac) = mac.filter(|&v| v != 0) {
                mac2unicode.insert(mac as i64, c.clone());
            }
            if let Some(win) = win.filter(|&v| v != 0) {
                win2unicode.insert(win as i64, c.clone());
            }
            if let Some(pdf) = pdf.filter(|&v| v != 0) {
                pdf2unicode.insert(pdf as i64, c.clone());
            }
        }
        EncodingDB {
            std2unicode: Rc::new(std2unicode),
            mac2unicode: Rc::new(mac2unicode),
            win2unicode: Rc::new(win2unicode),
            pdf2unicode: Rc::new(pdf2unicode),
        }
    }

    /// `EncodingDB.encodings.get(name, EncodingDB.std2unicode)`.
    fn encoding(&self, name: &str) -> Cid2Unicode {
        match name {
            "StandardEncoding" => self.std2unicode.clone(),
            "MacRomanEncoding" => self.mac2unicode.clone(),
            "WinAnsiEncoding" => self.win2unicode.clone(),
            "PDFDocEncoding" => self.pdf2unicode.clone(),
            _ => self.std2unicode.clone(),
        }
    }

    /// `EncodingDB.get_encoding(name, diff)`.
    pub fn get_encoding(name: &str, diff: Option<&[Value]>) -> Cid2Unicode {
        let mut cid2unicode = ENCODING_DB.with(|db| db.encoding(name));
        if let Some(diff) = diff
            && !diff.is_empty()
        {
            let mut copy = (*cid2unicode).clone();
            let mut cid: Value = Value::Int(0);
            for x in diff {
                if x.is_int() {
                    cid = x.clone();
                } else if let Value::Literal(lit) = x {
                    let key = cid.as_int().expect("cid is an int");
                    match name2unicode_arg(NameArg::from(&lit.name)) {
                        Ok(u) => {
                            copy.insert(key, u);
                        }
                        // `except (KeyError, ValueError)`: the only classes name2unicode raises.
                        Err(e) => {
                            debug_assert!(e.is_any(&[ExcKind::KeyError, ExcKind::ValueError]));
                            debug!("{}", e.msg);
                        }
                    }
                    // `cid += 1` (a bool becomes an int)
                    cid = Value::Int(key.wrapping_add(1));
                }
            }
            cid2unicode = Rc::new(copy);
        }
        cid2unicode
    }
}

#[cfg(test)]
mod tests {
    // Port of tests/test_encodingdb.py: tests based on the Adobe Glyph List Specification
    // (https://github.com/adobe-type-tools/agl-specification#2-the-mapping). While not in the
    // specification, lowercase unicode often occurs in pdf's, so lowercase variants are added.
    use super::*;
    use crate::psparser::LIT;

    fn s(x: &str) -> PyStr {
        PyStr::from(x)
    }

    fn key_error(name: &str) -> bool {
        name2unicode(name).unwrap_err().is(ExcKind::KeyError)
    }

    #[test]
    fn test_name2unicode_name_in_agl() {
        assert_eq!(name2unicode("Lcommaaccent").unwrap(), s("\u{013b}"));
    }

    #[test]
    fn test_name2unicode_uni() {
        assert_eq!(name2unicode("uni013B").unwrap(), s("\u{013b}"));
    }

    #[test]
    fn test_name2unicode_uni_lowercase() {
        assert_eq!(name2unicode("uni013b").unwrap(), s("\u{013b}"));
    }

    #[test]
    fn test_name2unicode_uni_with_sequence_of_digits() {
        assert_eq!(name2unicode("uni20AC0308").unwrap(), s("\u{20ac}\u{0308}"));
    }

    #[test]
    fn test_name2unicode_uni_with_sequence_of_digits_lowercase() {
        assert_eq!(name2unicode("uni20ac0308").unwrap(), s("\u{20ac}\u{0308}"));
    }

    #[test]
    fn test_name2unicode_uni_empty_string() {
        assert_eq!(name2unicode("uni20ac").unwrap(), s("\u{20ac}"));
    }

    #[test]
    fn test_name2unicode_uni_empty_string_long() {
        assert!(key_error("uniD801DC0C"));
    }

    #[test]
    fn test_name2unicode_uni_empty_string_long_lowercase() {
        assert!(key_error("uniD801DC0C"));
    }

    #[test]
    fn test_name2unicode_uni_pua() {
        assert_eq!(name2unicode("uniF6FB").unwrap(), s("\u{f6fb}"));
    }

    #[test]
    fn test_name2unicode_uni_pua_lowercase() {
        assert_eq!(name2unicode("unif6fb").unwrap(), s("\u{f6fb}"));
    }

    #[test]
    fn test_name2unicode_u_with_4_digits() {
        assert_eq!(name2unicode("u013B").unwrap(), s("\u{013b}"));
    }

    #[test]
    fn test_name2unicode_u_with_4_digits_lowercase() {
        assert_eq!(name2unicode("u013b").unwrap(), s("\u{013b}"));
    }

    #[test]
    fn test_name2unicode_u_with_5_digits() {
        assert_eq!(name2unicode("u1040C").unwrap(), s("\u{1040c}"));
    }

    #[test]
    fn test_name2unicode_u_with_5_digits_lowercase() {
        assert_eq!(name2unicode("u1040c").unwrap(), s("\u{1040c}"));
    }

    #[test]
    fn test_name2unicode_multiple_components() {
        assert_eq!(
            name2unicode("Lcommaaccent_uni20AC0308_u1040C.alternate").unwrap(),
            s("\u{013b}\u{20ac}\u{0308}\u{1040c}")
        );
    }

    #[test]
    fn test_name2unicode_multiple_components_lowercase() {
        assert_eq!(
            name2unicode("Lcommaaccent_uni20ac0308_u1040c.alternate").unwrap(),
            s("\u{013b}\u{20ac}\u{0308}\u{1040c}")
        );
    }

    #[test]
    fn test_name2unicode_foo() {
        assert!(key_error("foo"));
    }

    #[test]
    fn test_name2unicode_notdef() {
        assert!(key_error(".notdef"));
    }

    #[test]
    fn test_name2unicode_pua_ogoneksmall() {
        assert_eq!(name2unicode("Ogoneksmall").unwrap(), s("\u{f6fb}"));
    }

    #[test]
    fn test_name2unicode_overflow_error() {
        assert!(key_error("226215240241240240240240"));
    }

    #[test]
    fn test_get_encoding_with_invalid_differences() {
        // Invalid differences should be silently ignored
        // Regression test for https://github.com/pdfminer/pdfminer.six/issues/385
        let invalid_differences = vec![Value::Literal(LIT("ubuntu")), Value::Literal(LIT("1234"))];
        EncodingDB::get_encoding("StandardEncoding", Some(&invalid_differences));
    }

    /// Python behaviours checked against the reference: `str.strip("uni")`, `int(x, 16)` with a
    /// `0x` prefix, trailing whitespace and Unicode digits, and the ValueError cases.
    #[test]
    fn name2unicode_python_edge_cases() {
        assert!(name2unicode("uniform").unwrap_err().is(ExcKind::ValueError));
        assert!(name2unicode("uni00_4").unwrap_err().is(ExcKind::PDFKeyError));
        assert_eq!(name2unicode("u0041u").unwrap(), s("A"));
        assert!(name2unicode("uFFFFFF").unwrap_err().is(ExcKind::ValueError));
        assert!(name2unicode("uni-004").unwrap_err().is(ExcKind::PDFKeyError));
        assert_eq!(name2unicode("uni0x41").unwrap(), s("A"));
        assert_eq!(name2unicode("uni004 ").unwrap(), s("\u{4}"));
        assert_eq!(name2unicode("uni0\u{663}41").unwrap(), s("\u{341}"));
        assert!(name2unicode("uni00410-41").unwrap_err().is(ExcKind::ValueError));
        assert!(name2unicode("").unwrap_err().is(ExcKind::KeyError));
        let e = name2unicode_arg(NameArg::Other("bytes", "b'x'".into())).unwrap_err();
        assert!(e.is(ExcKind::PDFKeyError));
    }

    #[test]
    fn get_encoding_differences() {
        let diff = vec![
            Value::Int(65),
            Value::Literal(LIT("B")),
            Value::Literal(LIT("nonexistent")),
            Value::Literal(LIT("D")),
            Value::Bool(true),
            Value::Literal(LIT("E")),
            Value::Float(5.0),
            Value::Literal(LIT("F")),
        ];
        let e = EncodingDB::get_encoding("WinAnsiEncoding", Some(&diff));
        assert_eq!(e[&65], s("B"));
        assert_eq!(e[&66], s("B")); // unchanged: the failed name still advanced the cid
        assert_eq!(e[&67], s("D"));
        assert_eq!(e[&1], s("E"));
        assert_eq!(e[&2], s("F"));
        let std = EncodingDB::get_encoding("Nope", None);
        assert_eq!(std[&0o47], s("\u{2019}"));
    }
}
