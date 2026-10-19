//! Python's output streams and `str.encode(codec)`, as the converters and tools write to them.
//!
//! pdfminer writes either `str` (to a text stream: `sys.stdout`, `io.StringIO`, a file opened
//! with `open(path, "w")`) or `bytes` (to a binary stream: `sys.stdout.buffer`, `io.BytesIO`, a
//! file opened with `open(path, "wb")`), and decides which with
//! `PDFConverter._is_binary_stream(outfp)`. [`PyFile`] is such a stream; [`AnyIO`] is how the
//! port shares one between a converter and its caller (Python passes the same object around).
//!
//! What can differ between the two is what happens to text Python cannot encode (lone surrogates,
//! which pdfminer produces): a text stream encodes with its own encoding and error handler
//! (`sys.stdout` uses `surrogateescape` under the C and C.UTF-8 locales, `strict` otherwise), a
//! converter writing bytes encodes with its codec, strictly. A failed write writes nothing and
//! raises `UnicodeEncodeError`; what was written before stays.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use super::error::{ExcKind, PyErr, PyResult};
use super::pystr::PyStr;

// ---------------------------------------------------------------------------------------------
// Codecs

/// A codec of `str.encode`, by its canonical name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Utf8,
    Latin1,
    Ascii,
    Utf16,
    Utf16Le,
    Utf16Be,
    Utf32,
    Utf32Le,
    Utf32Be,
}

impl Codec {
    /// The name error messages use (`codec.name`).
    pub fn name(self) -> &'static str {
        match self {
            Codec::Utf8 => "utf-8",
            Codec::Latin1 => "latin-1",
            Codec::Ascii => "ascii",
            Codec::Utf16 => "utf-16",
            Codec::Utf16Le => "utf-16-le",
            Codec::Utf16Be => "utf-16-be",
            Codec::Utf32 => "utf-32",
            Codec::Utf32Le => "utf-32-le",
            Codec::Utf32Be => "utf-32-be",
        }
    }
}

/// `encodings.normalize_encoding(name.lower())`: runs of characters other than letters, digits
/// and `.` become one `_`, without leading or trailing `_`.
fn normalize_encoding(name: &str) -> String {
    let mut out = String::new();
    let mut punct = false;
    for c in name.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_alphanumeric() || c == '.' {
            if punct && !out.is_empty() {
                out.push('_');
            }
            out.push(c);
            punct = false;
        } else {
            punct = true;
        }
    }
    out
}

/// `codecs.lookup(name)`: `LookupError` for an unknown codec.
///
/// DIVERGE(codecs): only the UTF-8, Latin-1, ASCII, UTF-16 and UTF-32 codecs (with all their
/// Python aliases) are known; any other codec Python has (`cp1252`, `shift_jis`, ...) raises
/// `LookupError` here.
pub fn lookup(name: &str) -> PyResult<Codec> {
    let norm = normalize_encoding(name);
    let codec = match norm.as_str() {
        "utf_8" | "u8" | "utf" | "utf8" | "utf8_ucs2" | "utf8_ucs4" | "cp65001" => Some(Codec::Utf8),
        "latin_1" | "8859" | "cp819" | "csisolatin1" | "ibm819" | "iso8859" | "iso8859_1" | "iso_8859_1"
        | "iso_8859_1_1987" | "iso_ir_100" | "l1" | "latin" | "latin1" => Some(Codec::Latin1),
        "ascii" | "646" | "ansi_x3.4_1968" | "ansi_x3_4_1968" | "ansi_x3.4_1986" | "cp367" | "csascii" | "ibm367"
        | "iso646_us" | "iso_646.irv_1991" | "iso_ir_6" | "us" | "us_ascii" => Some(Codec::Ascii),
        "utf_16" | "u16" | "utf16" => Some(Codec::Utf16),
        "utf_16_le" | "unicodelittleunmarked" | "utf_16le" => Some(Codec::Utf16Le),
        "utf_16_be" | "unicodebigunmarked" | "utf_16be" => Some(Codec::Utf16Be),
        "utf_32" | "u32" | "utf32" => Some(Codec::Utf32),
        "utf_32_le" | "utf_32le" => Some(Codec::Utf32Le),
        "utf_32_be" | "utf_32be" => Some(Codec::Utf32Be),
        _ => None,
    };
    codec.ok_or_else(|| PyErr::new(ExcKind::LookupError, format!("unknown encoding: {name}")))
}

/// The error handlers pdfminer's streams use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Errors {
    Strict,
    /// Lone surrogates U+DC80..U+DCFF become the bytes 0x80..0xFF.
    SurrogateEscape,
}

fn is_surrogate(cp: u32) -> bool {
    (0xD800..=0xDFFF).contains(&cp)
}

/// How Python prints the character in a `UnicodeEncodeError` message.
fn char_escape(cp: u32) -> String {
    if cp <= 0xFF {
        format!("\\x{cp:02x}")
    } else if cp <= 0xFFFF {
        format!("\\u{cp:04x}")
    } else {
        format!("\\U{cp:08x}")
    }
}

/// `UnicodeEncodeError(codec, s, start, end, reason)` and its `str()`.
fn encode_error(codec: Codec, cps: &[u32], start: usize, end: usize, reason: &str) -> PyErr {
    let msg = if end == start + 1 {
        format!(
            "'{}' codec can't encode character '{}' in position {}: {}",
            codec.name(),
            char_escape(cps[start]),
            start,
            reason
        )
    } else {
        format!("'{}' codec can't encode characters in position {}-{}: {}", codec.name(), start, end - 1, reason)
    };
    PyErr::new(ExcKind::UnicodeEncodeError, msg)
}

fn push_utf8(out: &mut Vec<u8>, cp: u32) {
    let c = char::from_u32(cp).expect("not a surrogate");
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

/// `s.encode(codec, errors)`.
pub fn encode(s: &PyStr, codec: Codec, errors: Errors) -> PyResult<Vec<u8>> {
    if codec == Codec::Utf8
        && let Some(t) = s.as_str()
    {
        return Ok(t.as_bytes().to_vec());
    }
    let cps: Vec<u32> = s.code_points().collect();
    let mut out = Vec::with_capacity(cps.len());
    match codec {
        Codec::Utf8 | Codec::Latin1 | Codec::Ascii => {
            // A run of unencodable characters is handed to the error handler at once.
            let bad = |cp: u32| match codec {
                Codec::Utf8 => is_surrogate(cp),
                Codec::Latin1 => cp >= 0x100,
                _ => cp >= 0x80,
            };
            let mut i = 0;
            while i < cps.len() {
                let cp = cps[i];
                if !bad(cp) {
                    if codec == Codec::Utf8 {
                        push_utf8(&mut out, cp);
                    } else {
                        out.push(cp as u8);
                    }
                    i += 1;
                    continue;
                }
                let mut end = i;
                while end < cps.len() && bad(cps[end]) {
                    end += 1;
                }
                let mut start = i;
                if errors == Errors::SurrogateEscape {
                    while start < end && (0xDC80..=0xDCFF).contains(&cps[start]) {
                        out.push((cps[start] & 0xFF) as u8);
                        start += 1;
                    }
                }
                if start < end {
                    let reason = match codec {
                        Codec::Utf8 => "surrogates not allowed".to_string(),
                        Codec::Latin1 => "ordinal not in range(256)".to_string(),
                        _ => "ordinal not in range(128)".to_string(),
                    };
                    return Err(encode_error(codec, &cps, start, end, &reason));
                }
                i = end;
            }
        }
        Codec::Utf16 | Codec::Utf16Le | Codec::Utf16Be => {
            let be = codec == Codec::Utf16Be;
            let unit = |out: &mut Vec<u8>, u: u16| {
                if be { out.extend_from_slice(&u.to_be_bytes()) } else { out.extend_from_slice(&u.to_le_bytes()) }
            };
            if codec == Codec::Utf16 {
                unit(&mut out, 0xFEFF);
            }
            for (i, &cp) in cps.iter().enumerate() {
                if is_surrogate(cp) {
                    if errors == Errors::SurrogateEscape && (0xDC80..=0xDCFF).contains(&cp) {
                        out.push((cp & 0xFF) as u8);
                        continue;
                    }
                    return Err(encode_error(codec, &cps, i, i + 1, "surrogates not allowed"));
                }
                if cp >= 0x10000 {
                    let v = cp - 0x10000;
                    unit(&mut out, 0xD800 | (v >> 10) as u16);
                    unit(&mut out, 0xDC00 | (v & 0x3FF) as u16);
                } else {
                    unit(&mut out, cp as u16);
                }
            }
        }
        Codec::Utf32 | Codec::Utf32Le | Codec::Utf32Be => {
            let be = codec == Codec::Utf32Be;
            let unit = |out: &mut Vec<u8>, u: u32| {
                if be { out.extend_from_slice(&u.to_be_bytes()) } else { out.extend_from_slice(&u.to_le_bytes()) }
            };
            if codec == Codec::Utf32 {
                unit(&mut out, 0xFEFF);
            }
            for (i, &cp) in cps.iter().enumerate() {
                if is_surrogate(cp) {
                    if errors == Errors::SurrogateEscape && (0xDC80..=0xDCFF).contains(&cp) {
                        out.push((cp & 0xFF) as u8);
                        continue;
                    }
                    return Err(encode_error(codec, &cps, i, i + 1, "surrogates not allowed"));
                }
                unit(&mut out, cp);
            }
        }
    }
    Ok(out)
}

/// `s.encode(name)` (strict), looking the codec up first.
pub fn encode_str(s: &PyStr, name: &str) -> PyResult<Vec<u8>> {
    encode(s, lookup(name)?, Errors::Strict)
}

// ---------------------------------------------------------------------------------------------
// Streams

/// A Python file object that pdfminer writes to.
pub trait PyFile {
    /// `PDFConverter._is_binary_stream(self)`: whether the stream takes `bytes`.
    fn is_binary(&self) -> bool;

    /// `self.write(s)` with a `str`.
    fn write_str(&mut self, s: &PyStr) -> PyResult<()>;

    /// `self.write(b)` with `bytes`.
    fn write_bytes(&mut self, b: &[u8]) -> PyResult<()>;

    /// `self.flush()`.
    fn flush(&mut self) -> PyResult<()> {
        Ok(())
    }
}

/// A shared stream (Python passes the same file object to a converter and keeps it).
pub type AnyIO = Rc<RefCell<dyn PyFile>>;

fn bytes_to_text_error() -> PyErr {
    PyErr::new(ExcKind::TypeError, "write() argument must be str, not bytes")
}

fn str_to_binary_error() -> PyErr {
    PyErr::new(ExcKind::TypeError, "a bytes-like object is required, not 'str'")
}

/// An OS error, as Python reports it (`[Errno N] text`).
pub fn os_error(e: &std::io::Error) -> PyErr {
    let kind = match e.kind() {
        std::io::ErrorKind::NotFound => ExcKind::FileNotFoundError,
        std::io::ErrorKind::AlreadyExists => ExcKind::FileExistsError,
        std::io::ErrorKind::PermissionDenied => ExcKind::PermissionError,
        std::io::ErrorKind::BrokenPipe => ExcKind::BrokenPipeError,
        std::io::ErrorKind::IsADirectory => ExcKind::IsADirectoryError,
        std::io::ErrorKind::NotADirectory => ExcKind::NotADirectoryError,
        _ => ExcKind::OSError,
    };
    let msg = match e.raw_os_error() {
        Some(code) => {
            let text = e.to_string();
            // Rust: "No such file or directory (os error 2)"; Python: "[Errno 2] No such file ...".
            let text = text.split(" (os error").next().unwrap_or(&text).to_string();
            format!("[Errno {code}] {text}")
        }
        None => e.to_string(),
    };
    PyErr::new(kind, msg)
}

/// `os_error` for an operation on `path`: `[Errno 2] No such file or directory: 'path'`.
pub fn os_error_path(e: &std::io::Error, path: &str) -> PyErr {
    let mut err = os_error(e);
    if e.raw_os_error().is_some() {
        err.msg = format!("{}: {}", err.msg, super::fmt::str_repr(path));
    }
    err
}

/// `io.StringIO()`.
#[derive(Default)]
pub struct StringIO {
    buf: PyStr,
}

impl StringIO {
    pub fn new() -> Self {
        StringIO::default()
    }

    /// `getvalue()`.
    pub fn getvalue(&self) -> PyStr {
        self.buf.clone()
    }
}

impl PyFile for StringIO {
    fn is_binary(&self) -> bool {
        false
    }
    fn write_str(&mut self, s: &PyStr) -> PyResult<()> {
        self.buf.push_pystr(s);
        Ok(())
    }
    fn write_bytes(&mut self, _b: &[u8]) -> PyResult<()> {
        Err(PyErr::new(ExcKind::TypeError, "string argument expected, got 'bytes'"))
    }
}

/// `io.BytesIO()` used for output.
#[derive(Default)]
pub struct BytesOut {
    buf: Vec<u8>,
}

impl BytesOut {
    pub fn new() -> Self {
        BytesOut::default()
    }

    /// `getvalue()`.
    pub fn getvalue(&self) -> &[u8] {
        &self.buf
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }
}

impl PyFile for BytesOut {
    fn is_binary(&self) -> bool {
        true
    }
    fn write_str(&mut self, _s: &PyStr) -> PyResult<()> {
        Err(str_to_binary_error())
    }
    fn write_bytes(&mut self, b: &[u8]) -> PyResult<()> {
        self.buf.extend_from_slice(b);
        Ok(())
    }
}

/// A binary stream over a writer: `open(path, "wb")`, `sys.stdout.buffer`.
pub struct BinaryFile<W: Write> {
    pub inner: W,
}

impl<W: Write> BinaryFile<W> {
    pub fn new(inner: W) -> Self {
        BinaryFile { inner }
    }
}

impl<W: Write> PyFile for BinaryFile<W> {
    fn is_binary(&self) -> bool {
        true
    }
    fn write_str(&mut self, _s: &PyStr) -> PyResult<()> {
        Err(str_to_binary_error())
    }
    fn write_bytes(&mut self, b: &[u8]) -> PyResult<()> {
        self.inner.write_all(b).map_err(|e| os_error(&e))
    }
    fn flush(&mut self) -> PyResult<()> {
        self.inner.flush().map_err(|e| os_error(&e))
    }
}

/// A text stream over a writer (`io.TextIOWrapper`): `sys.stdout`, `open(path, "w")`. A `str`
/// is encoded with the stream's encoding and error handler when written; a write that fails to
/// encode writes nothing.
pub struct TextFile<W: Write> {
    pub inner: W,
    pub encoding: Codec,
    pub errors: Errors,
}

impl<W: Write> TextFile<W> {
    pub fn new(inner: W, encoding: Codec, errors: Errors) -> Self {
        TextFile { inner, encoding, errors }
    }
}

impl<W: Write> PyFile for TextFile<W> {
    fn is_binary(&self) -> bool {
        false
    }
    fn write_str(&mut self, s: &PyStr) -> PyResult<()> {
        let b = encode(s, self.encoding, self.errors)?;
        self.inner.write_all(&b).map_err(|e| os_error(&e))
    }
    fn write_bytes(&mut self, _b: &[u8]) -> PyResult<()> {
        Err(bytes_to_text_error())
    }
    fn flush(&mut self) -> PyResult<()> {
        self.inner.flush().map_err(|e| os_error(&e))
    }
}

/// The encoding and error handler of Python's `sys.stdout` in the current environment.
///
/// `PYTHONIOENCODING=encoding[:errors]` decides first; otherwise the encoding is the locale's
/// (UTF-8 here) and the error handler is `surrogateescape` in UTF-8 mode (`PYTHONUTF8=1`) and
/// under the C, POSIX and C.UTF-8 locales (CPython's `config_get_stdio_errors`), `strict`
/// otherwise. DIVERGE(stdout-locale): a locale whose encoding is not UTF-8 still gets UTF-8.
pub fn stdout_encoding() -> (Codec, Errors) {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut encoding = Codec::Utf8;
    let mut errors: Option<Errors> = None;
    if let Some(v) = env("PYTHONIOENCODING") {
        let (enc, err) = match v.split_once(':') {
            Some((a, b)) => (a.to_string(), Some(b.to_string())),
            None => (v.clone(), None),
        };
        if !enc.is_empty()
            && let Ok(c) = lookup(&enc)
        {
            encoding = c;
        }
        if let Some(err) = err.filter(|e| !e.is_empty()) {
            errors = Some(if err == "surrogateescape" { Errors::SurrogateEscape } else { Errors::Strict });
        }
    }
    let errors = errors.unwrap_or_else(|| {
        if env("PYTHONUTF8").as_deref() == Some("1") {
            return Errors::SurrogateEscape;
        }
        // setlocale(LC_CTYPE, ""): LC_ALL, then LC_CTYPE, then LANG; "C" when none is set.
        let loc = env("LC_ALL").or_else(|| env("LC_CTYPE")).or_else(|| env("LANG")).unwrap_or_else(|| "C".into());
        if matches!(loc.as_str(), "C" | "POSIX" | "C.UTF-8" | "C.utf8" | "UTF-8") {
            Errors::SurrogateEscape
        } else {
            Errors::Strict
        }
    });
    (encoding, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Messages from Python 3.14.
    #[test]
    fn encode_errors_match_python() {
        let s = PyStr::from_code_points([0x61, 0xDC80, 0xD800, 0x62]);
        let e = encode(&s, Codec::Utf8, Errors::SurrogateEscape).unwrap_err();
        assert_eq!(e.msg, "'utf-8' codec can't encode character '\\ud800' in position 2: surrogates not allowed");
        let e = encode(&PyStr::from("\u{4e2d}\u{4e2d}x"), Codec::Latin1, Errors::Strict).unwrap_err();
        assert_eq!(e.msg, "'latin-1' codec can't encode characters in position 0-1: ordinal not in range(256)");
        let e = encode(&PyStr::from("\u{1f600}"), Codec::Ascii, Errors::Strict).unwrap_err();
        assert_eq!(
            e.msg,
            "'ascii' codec can't encode character '\\U0001f600' in position 0: ordinal not in range(128)"
        );
        let ok = encode(&PyStr::from_code_points([0x62, 0xDC80, 0xDC81, 0x63]), Codec::Utf8, Errors::SurrogateEscape);
        assert_eq!(ok.unwrap(), b"b\x80\x81c");
        assert_eq!(lookup("foo").unwrap_err().msg, "unknown encoding: foo");
        assert_eq!(lookup("UTF8").unwrap(), Codec::Utf8);
        assert_eq!(lookup("ISO-8859-1").unwrap(), Codec::Latin1);
        assert_eq!(encode(&PyStr::from("\u{e9}"), Codec::Utf16, Errors::Strict).unwrap(), b"\xff\xfe\xe9\x00");
    }
}
