//! Port of `pdfminer/psparser.py`.
//!
//! Python's class hierarchy (`PSBaseParser` <- `PSStackParser` <- `PDFParser`, `CMapParser`, ...)
//! becomes: the state in plain structs ([`PSBaseParser`], [`PSStackParser`]) and the methods that
//! subclasses override (`seek`, `fillbuf`, `flush`, `do_keyword`) in the [`PSStackParserImpl`]
//! trait, whose provided methods (`nexttoken`, `nextobject`, ...) are the inherited algorithms.

use std::collections::VecDeque;
use std::rc::Rc;

use log::debug;

use crate::pyrt::ctype::{byte_isalpha, byte_isdigit, byte_isspace};
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::{bytes_repr, str_repr};
use crate::pyrt::io::BytesIO;
use crate::pyrt::value::{Dict, Value};
use crate::settings;
use crate::utils::choplist;

/// `PSLiteral.name`: a `str`, or `bytes` when the name is not valid UTF-8.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NameType {
    Str(Rc<str>),
    Bytes(Rc<[u8]>),
}

/// A PostScript literal (`/Name`).
///
/// Python interns literals (`PSLiteralTable.intern`) and compares them with `is`; two interned
/// literals are the same object exactly when their names are equal (same type and value), which
/// is what `==` here compares.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PSLiteral {
    pub name: NameType,
}

impl PSLiteral {
    pub fn new(name: NameType) -> Self {
        PSLiteral { name }
    }

    /// The name when it is a `str`.
    pub fn name_str(&self) -> Option<&str> {
        match &self.name {
            NameType::Str(s) => Some(s),
            NameType::Bytes(_) => None,
        }
    }

    /// `lit is LIT(name)`.
    pub fn is(&self, name: &str) -> bool {
        self.name_str() == Some(name)
    }

    /// `repr(lit)`: `/'Name'` (or `/b'...'`).
    pub fn repr(&self) -> String {
        match &self.name {
            NameType::Str(s) => format!("/{}", str_repr(s)),
            NameType::Bytes(b) => format!("/{}", bytes_repr(b)),
        }
    }
}

/// A PostScript keyword (an operator or delimiter); compared like [`PSLiteral`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PSKeyword {
    pub name: Rc<[u8]>,
}

impl PSKeyword {
    /// `kwd is KWD(name)`.
    pub fn is(&self, name: &[u8]) -> bool {
        &*self.name == name
    }

    /// `repr(kwd)`: `/b'name'`.
    pub fn repr(&self) -> String {
        format!("/{}", bytes_repr(&self.name))
    }
}

/// `LIT(name)` (`PSLiteralTable.intern`) for a `str` name.
#[allow(non_snake_case)]
pub fn LIT(name: &str) -> PSLiteral {
    PSLiteral { name: NameType::Str(Rc::from(name)) }
}

/// `LIT(name)` for a `bytes` name.
#[allow(non_snake_case)]
pub fn LIT_bytes(name: &[u8]) -> PSLiteral {
    PSLiteral { name: NameType::Bytes(Rc::from(name)) }
}

/// `KWD(name)` (`PSKeywordTable.intern`).
#[allow(non_snake_case)]
pub fn KWD(name: &[u8]) -> PSKeyword {
    PSKeyword { name: Rc::from(name) }
}

pub const KEYWORD_PROC_BEGIN: &[u8] = b"{";
pub const KEYWORD_PROC_END: &[u8] = b"}";
pub const KEYWORD_ARRAY_BEGIN: &[u8] = b"[";
pub const KEYWORD_ARRAY_END: &[u8] = b"]";
pub const KEYWORD_DICT_BEGIN: &[u8] = b"<<";
pub const KEYWORD_DICT_END: &[u8] = b">>";

/// `str(b, "utf-8")` (strict): `None` when Python would raise `UnicodeDecodeError`.
fn utf8_strict(b: &[u8]) -> Option<&str> {
    std::str::from_utf8(b).ok()
}

/// `literal_name(x)`.
pub fn literal_name(x: &Value) -> PyResult<String> {
    if let Value::Literal(lit) = x {
        return Ok(match &lit.name {
            NameType::Str(s) => s.to_string(),
            NameType::Bytes(b) => match utf8_strict(b) {
                Some(s) => s.to_string(),
                None => bytes_repr(b),
            },
        });
    }
    if settings::STRICT {
        return Err(PyErr::new(ExcKind::PSTypeError, format!("Literal required: {}", x.repr())));
    }
    Ok(x.py_str())
}

/// `literal_name(lit)` for a value known to be a literal.
pub fn literal_name_of(lit: &PSLiteral) -> String {
    match &lit.name {
        NameType::Str(s) => s.to_string(),
        NameType::Bytes(b) => match utf8_strict(b) {
            Some(s) => s.to_string(),
            None => bytes_repr(b),
        },
    }
}

/// `keyword_name(x)`: the keyword's name as `str` (UTF-8, invalid bytes dropped); any other
/// object is returned as is (non-strict mode).
pub fn keyword_name(x: &Value) -> PyResult<Value> {
    match x {
        Value::Keyword(k) => Ok(Value::str(&crate::pyrt::codecs::utf8_decode_ignore(&k.name))),
        _ => {
            if settings::STRICT {
                return Err(PyErr::new(ExcKind::PSTypeError, format!("Keyword required: {}", x.repr())));
            }
            Ok(x.clone())
        }
    }
}

/// `keyword_name(kwd)` for a value known to be a keyword.
pub fn keyword_name_of(k: &PSKeyword) -> String {
    crate::pyrt::codecs::utf8_decode_ignore(&k.name)
}

// Character classes of the regexes in psparser.py (bytes patterns: `\s` is ASCII whitespace).

/// `EOL = re.compile(rb"[\r\n]")`
#[inline]
fn is_eol(c: u8) -> bool {
    c == b'\r' || c == b'\n'
}

/// `END_LITERAL` / `END_KEYWORD = re.compile(rb"[#/%\[\]()<>{}\s]")`
#[inline]
fn is_end_literal(c: u8) -> bool {
    matches!(c, b'#' | b'/' | b'%' | b'[' | b']' | b'(' | b')' | b'<' | b'>' | b'{' | b'}') || byte_isspace(c)
}

/// `END_HEX_STRING = re.compile(rb"[^\s0-9a-fA-F]")`
#[inline]
fn is_end_hex_string(c: u8) -> bool {
    !(byte_isspace(c) || c.is_ascii_hexdigit())
}

/// `END_NUMBER = re.compile(rb"[^0-9]")`
#[inline]
fn is_end_number(c: u8) -> bool {
    !c.is_ascii_digit()
}

/// `END_STRING = re.compile(rb"[()\134]")`
#[inline]
fn is_end_string(c: u8) -> bool {
    matches!(c, b'(' | b')' | b'\\')
}

/// `ESC_STRING`
fn esc_string(c: u8) -> Option<u8> {
    match c {
        b'b' => Some(8),
        b't' => Some(9),
        b'n' => Some(10),
        b'f' => Some(12),
        b'r' => Some(13),
        b'(' => Some(40),
        b')' => Some(41),
        b'\\' => Some(92),
        _ => None,
    }
}

/// `pattern.search(s, i)` for a one-byte class: the index of the first match at or after `i`.
#[inline]
fn search(s: &[u8], i: usize, pred: impl Fn(u8) -> bool) -> Option<usize> {
    s.get(i..)?.iter().position(|&c| pred(c)).map(|p| p + i)
}

/// Which `_parse_*` method `self._parse1` points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parse1 {
    Main,
    Comment,
    Literal,
    LiteralHex,
    Number,
    Float,
    Keyword,
    String,
    String1,
    Wopen,
    Wclose,
    HexString,
}

/// `PSBaseParser`: tokenization state.
pub struct PSBaseParser {
    /// `self.fp`; `None` only for `PDFContentParser` between streams.
    pub fp: Option<BytesIO>,
    pub eof: bool,
    pub bufpos: i64,
    pub buf: Vec<u8>,
    pub charpos: usize,
    pub parse1: Parse1,
    pub curtoken: Vec<u8>,
    pub curtokenpos: i64,
    pub tokens: VecDeque<(i64, Value)>,
    pub hex: Vec<u8>,
    pub oct: Vec<u8>,
    pub paren: i64,
}

pub const BUFSIZ: usize = 4096;

impl PSBaseParser {
    /// `PSBaseParser.__init__(fp)` (which also calls `seek(0)` when `fp` is set).
    pub fn new(fp: Option<BytesIO>) -> Self {
        let mut p = PSBaseParser {
            fp,
            eof: false,
            bufpos: 0,
            buf: Vec::new(),
            charpos: 0,
            parse1: Parse1::Main,
            curtoken: Vec::new(),
            curtokenpos: 0,
            tokens: VecDeque::new(),
            hex: Vec::new(),
            oct: Vec::new(),
            paren: 0,
        };
        p.base_seek(0).expect("seek(0) cannot fail");
        p
    }

    /// `PSBaseParser.seek(pos)`.
    pub fn base_seek(&mut self, pos: i64) -> PyResult<()> {
        debug!("seek: {pos:?}");
        if let Some(fp) = self.fp.as_mut() {
            fp.seek(pos)?;
        }
        // reset the status for nextline()
        self.bufpos = pos;
        self.buf.clear();
        self.charpos = 0;
        // reset the status for nexttoken()
        self.parse1 = Parse1::Main;
        self.curtoken.clear();
        self.curtokenpos = 0;
        self.tokens.clear();
        self.eof = false;
        Ok(())
    }

    fn fp(&mut self) -> &mut BytesIO {
        self.fp.as_mut().expect("parser has no file")
    }

    /// `PSBaseParser.fillbuf()`.
    pub fn base_fillbuf(&mut self) -> PyResult<bool> {
        if self.charpos < self.buf.len() {
            return Ok(false);
        }
        // fetch next chunk.
        self.bufpos = self.fp().tell();
        let chunk = self.fp().read(BUFSIZ as i64).to_vec();
        self.buf = chunk;
        if self.buf.is_empty() {
            return Err(PyErr::new(ExcKind::PSEOF, "Unexpected EOF"));
        }
        self.charpos = 0;
        Ok(false)
    }

    fn add_token(&mut self, obj: Value) {
        self.tokens.push_back((self.curtokenpos, obj));
    }

    /// Runs the current `self._parse1(s, i)`.
    pub fn parse1(&mut self, s: &[u8], i: usize) -> PyResult<usize> {
        match self.parse1 {
            Parse1::Main => Ok(self.parse_main(s, i)),
            Parse1::Comment => Ok(self.parse_comment(s, i)),
            Parse1::Literal => Ok(self.parse_literal(s, i)),
            Parse1::LiteralHex => Ok(self.parse_literal_hex(s, i)),
            Parse1::Number => Ok(self.parse_number(s, i)),
            Parse1::Float => Ok(self.parse_float(s, i)),
            Parse1::Keyword => Ok(self.parse_keyword(s, i)),
            Parse1::String => Ok(self.parse_string(s, i)),
            Parse1::String1 => self.parse_string_1(s, i),
            Parse1::Wopen => Ok(self.parse_wopen(s, i)),
            Parse1::Wclose => Ok(self.parse_wclose(s, i)),
            Parse1::HexString => Ok(self.parse_hexstring(s, i)),
        }
    }

    fn parse_main(&mut self, s: &[u8], i: usize) -> usize {
        let Some(j) = search(s, i, |c| !byte_isspace(c)) else {
            return s.len();
        };
        let c = s[j];
        self.curtokenpos = self.bufpos + j as i64;
        if c == b'%' {
            self.curtoken = vec![b'%'];
            self.parse1 = Parse1::Comment;
            j + 1
        } else if c == b'/' {
            self.curtoken.clear();
            self.parse1 = Parse1::Literal;
            j + 1
        } else if c == b'-' || c == b'+' || byte_isdigit(c) {
            self.curtoken = vec![c];
            self.parse1 = Parse1::Number;
            j + 1
        } else if c == b'.' {
            self.curtoken = vec![c];
            self.parse1 = Parse1::Float;
            j + 1
        } else if byte_isalpha(c) {
            self.curtoken = vec![c];
            self.parse1 = Parse1::Keyword;
            j + 1
        } else if c == b'(' {
            self.curtoken.clear();
            self.paren = 1;
            self.parse1 = Parse1::String;
            j + 1
        } else if c == b'<' {
            self.curtoken.clear();
            self.parse1 = Parse1::Wopen;
            j + 1
        } else if c == b'>' {
            self.curtoken.clear();
            self.parse1 = Parse1::Wclose;
            j + 1
        } else if c == 0 {
            j + 1
        } else {
            self.add_token(Value::Keyword(KWD(&[c])));
            j + 1
        }
    }

    fn parse_comment(&mut self, s: &[u8], i: usize) -> usize {
        let Some(j) = search(s, i, is_eol) else {
            self.curtoken.extend_from_slice(&s[i..]);
            return s.len();
        };
        self.curtoken.extend_from_slice(&s[i..j]);
        self.parse1 = Parse1::Main;
        // We ignore comments.
        j
    }

    fn parse_literal(&mut self, s: &[u8], i: usize) -> usize {
        let Some(j) = search(s, i, is_end_literal) else {
            self.curtoken.extend_from_slice(&s[i..]);
            return s.len();
        };
        self.curtoken.extend_from_slice(&s[i..j]);
        let c = s[j];
        if c == b'#' {
            self.hex.clear();
            self.parse1 = Parse1::LiteralHex;
            return j + 1;
        }
        let name = match utf8_strict(&self.curtoken) {
            Some(n) => NameType::Str(Rc::from(n)),
            None => NameType::Bytes(Rc::from(&self.curtoken[..])),
        };
        self.add_token(Value::Literal(PSLiteral::new(name)));
        self.parse1 = Parse1::Main;
        j
    }

    fn parse_literal_hex(&mut self, s: &[u8], i: usize) -> usize {
        let c = s.get(i).copied();
        if let Some(c) = c
            && c.is_ascii_hexdigit()
            && self.hex.len() < 2
        {
            self.hex.push(c);
            return i + 1;
        }
        if !self.hex.is_empty() {
            let v = u8::from_str_radix(std::str::from_utf8(&self.hex).unwrap(), 16).unwrap();
            self.curtoken.push(v);
        }
        self.parse1 = Parse1::Literal;
        i
    }

    fn parse_number(&mut self, s: &[u8], i: usize) -> usize {
        let Some(j) = search(s, i, is_end_number) else {
            self.curtoken.extend_from_slice(&s[i..]);
            return s.len();
        };
        self.curtoken.extend_from_slice(&s[i..j]);
        let c = s[j];
        if c == b'.' {
            self.curtoken.push(c);
            self.parse1 = Parse1::Float;
            return j + 1;
        }
        // with contextlib.suppress(ValueError): self._add_token(int(self._curtoken))
        if let Some(v) = parse_int_token(&self.curtoken) {
            self.add_token(v);
        }
        self.parse1 = Parse1::Main;
        j
    }

    fn parse_float(&mut self, s: &[u8], i: usize) -> usize {
        let Some(j) = search(s, i, is_end_number) else {
            self.curtoken.extend_from_slice(&s[i..]);
            return s.len();
        };
        self.curtoken.extend_from_slice(&s[i..j]);
        // with contextlib.suppress(ValueError): self._add_token(float(self._curtoken))
        if let Some(v) = parse_float_token(&self.curtoken) {
            self.add_token(Value::Float(v));
        }
        self.parse1 = Parse1::Main;
        j
    }

    fn parse_keyword(&mut self, s: &[u8], i: usize) -> usize {
        let j = match search(s, i, is_end_literal) {
            Some(j) => {
                self.curtoken.extend_from_slice(&s[i..j]);
                j
            }
            None => {
                self.curtoken.extend_from_slice(&s[i..]);
                return s.len();
            }
        };
        let token = if self.curtoken == b"true" {
            Value::Bool(true)
        } else if self.curtoken == b"false" {
            Value::Bool(false)
        } else {
            Value::Keyword(KWD(&self.curtoken))
        };
        self.add_token(token);
        self.parse1 = Parse1::Main;
        j
    }

    fn parse_string(&mut self, s: &[u8], i: usize) -> usize {
        let Some(j) = search(s, i, is_end_string) else {
            self.curtoken.extend_from_slice(&s[i..]);
            return s.len();
        };
        self.curtoken.extend_from_slice(&s[i..j]);
        let c = s[j];
        if c == b'\\' {
            self.oct.clear();
            self.parse1 = Parse1::String1;
            return j + 1;
        }
        if c == b'(' {
            self.paren += 1;
            self.curtoken.push(c);
            return j + 1;
        }
        if c == b')' {
            self.paren -= 1;
            if self.paren != 0 {
                // WTF, they said balanced parens need no special treatment.
                self.curtoken.push(c);
                return j + 1;
            }
        }
        let tok = Value::bytes(&self.curtoken);
        self.add_token(tok);
        self.parse1 = Parse1::Main;
        j + 1
    }

    /// Parse literal strings (PDF Reference 3.2.3).
    fn parse_string_1(&mut self, s: &[u8], mut i: usize) -> PyResult<usize> {
        let c = s.get(i).copied();
        if let Some(c) = c
            && (b'0'..=b'7').contains(&c)
            && self.oct.len() < 3
        {
            self.oct.push(c);
            return Ok(i + 1);
        } else if !self.oct.is_empty() {
            let chrcode = u32::from_str_radix(std::str::from_utf8(&self.oct).unwrap(), 8).unwrap();
            if chrcode >= 256 {
                return Err(PyErr::new(
                    ExcKind::AssertionError,
                    format!("Invalid octal {} ({})", bytes_repr(&self.oct), chrcode),
                ));
            }
            self.curtoken.push(chrcode as u8);
            self.parse1 = Parse1::String;
            return Ok(i);
        } else if let Some(v) = c.and_then(esc_string) {
            self.curtoken.push(v);
        } else if c == Some(b'\r') && s.len() > i + 1 && s[i + 1] == b'\n' {
            // If current and next character is \r\n skip both because enters
            // after a \ are ignored
            i += 1;
        }
        // default action
        self.parse1 = Parse1::String;
        Ok(i + 1)
    }

    fn parse_wopen(&mut self, s: &[u8], mut i: usize) -> usize {
        if s.get(i) == Some(&b'<') {
            self.add_token(Value::Keyword(KWD(KEYWORD_DICT_BEGIN)));
            self.parse1 = Parse1::Main;
            i += 1;
        } else {
            self.parse1 = Parse1::HexString;
        }
        i
    }

    fn parse_wclose(&mut self, s: &[u8], mut i: usize) -> usize {
        if s.get(i) == Some(&b'>') {
            self.add_token(Value::Keyword(KWD(KEYWORD_DICT_END)));
            i += 1;
        }
        self.parse1 = Parse1::Main;
        i
    }

    fn parse_hexstring(&mut self, s: &[u8], i: usize) -> usize {
        let Some(j) = search(s, i, is_end_hex_string) else {
            self.curtoken.extend_from_slice(&s[i..]);
            return s.len();
        };
        self.curtoken.extend_from_slice(&s[i..j]);
        // HEX_PAIR.sub(lambda m: bytes((int(m.group(0), 16),)), SPC.sub(b"", self._curtoken))
        let compact: Vec<u8> = self.curtoken.iter().copied().filter(|&c| !byte_isspace(c)).collect();
        let token = hex_pair_sub(&compact);
        self.add_token(Value::bytes(&token));
        self.parse1 = Parse1::Main;
        j
    }
}

/// `HEX_PAIR.sub(...)` with `HEX_PAIR = rb"[0-9a-fA-F]{2}|."`: each pair of hex digits becomes a
/// byte, and any other single byte (except `\n`, which `.` does not match and is kept) is
/// converted with `int(c, 16)`, which only succeeds for a hex digit. The input here only holds hex
/// digits (whitespace was removed), so a lone trailing digit `d` becomes the byte `int(d, 16)`.
fn hex_pair_sub(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 2 + 1);
    let mut i = 0;
    while i < s.len() {
        if i + 1 < s.len() && s[i].is_ascii_hexdigit() && s[i + 1].is_ascii_hexdigit() {
            out.push(u8::from_str_radix(std::str::from_utf8(&s[i..i + 2]).unwrap(), 16).unwrap());
            i += 2;
        } else {
            out.push(u8::from_str_radix(std::str::from_utf8(&s[i..i + 1]).unwrap(), 16).unwrap());
            i += 1;
        }
    }
    out
}

/// `int(token)` for a number token (`[+-]?[0-9]*`); `None` where Python raises `ValueError`.
fn parse_int_token(tok: &[u8]) -> Option<Value> {
    let s = std::str::from_utf8(tok).ok()?;
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() {
        return None;
    }
    match s.parse::<i64>() {
        Ok(v) => Some(Value::Int(v)),
        // A Python int beyond i64: recorded divergence (kept as the nearest float).
        Err(_) => s.parse::<f64>().ok().map(Value::Float),
    }
}

/// `float(token)` for a float token (`[+-]?[0-9]*\.[0-9]*`); `None` where Python raises
/// `ValueError` (no digits at all).
fn parse_float_token(tok: &[u8]) -> Option<f64> {
    let s = std::str::from_utf8(tok).ok()?;
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if !body.bytes().any(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse::<f64>().ok()
}

/// One entry of the stack: `(pos, obj)`.
pub type PSStackEntry = (i64, Value);

/// `PSStackParser`: tokenizer state plus the object stack.
pub struct PSStackParser {
    pub base: PSBaseParser,
    pub context: Vec<(i64, Option<&'static str>, Vec<PSStackEntry>)>,
    pub curtype: Option<&'static str>,
    pub curstack: Vec<PSStackEntry>,
    pub results: VecDeque<PSStackEntry>,
}

impl PSStackParser {
    pub fn new(fp: Option<BytesIO>) -> Self {
        let mut p = PSStackParser {
            base: PSBaseParser::new(fp),
            context: Vec::new(),
            curtype: None,
            curstack: Vec::new(),
            results: VecDeque::new(),
        };
        p.reset();
        p
    }

    pub fn reset(&mut self) {
        self.context.clear();
        self.curtype = None;
        self.curstack.clear();
        self.results.clear();
    }

    /// `PSStackParser.seek(pos)`.
    pub fn stack_seek(&mut self, pos: i64) -> PyResult<()> {
        self.base.base_seek(pos)?;
        self.reset();
        Ok(())
    }

    pub fn push(&mut self, obj: PSStackEntry) {
        self.curstack.push(obj);
    }

    /// `self.pop(n)`: the last `n` entries (fewer if the stack is shorter; Python's `[-n:]`,
    /// where `n == 0` takes everything).
    pub fn pop(&mut self, n: usize) -> Vec<PSStackEntry> {
        let len = self.curstack.len();
        let start = if n == 0 { 0 } else { len.saturating_sub(n) };
        self.curstack.split_off(start)
    }

    pub fn popall(&mut self) -> Vec<PSStackEntry> {
        std::mem::take(&mut self.curstack)
    }

    pub fn add_results(&mut self, objs: Vec<PSStackEntry>) {
        debug!("add_results: {objs:?}");
        self.results.extend(objs);
    }

    pub fn start_type(&mut self, pos: i64, type_: &'static str) {
        let curstack = std::mem::take(&mut self.curstack);
        self.context.push((pos, self.curtype, curstack));
        self.curtype = Some(type_);
        debug!("start_type: pos={pos:?}, type={type_:?}");
    }

    pub fn end_type(&mut self, type_: &'static str) -> PyResult<(i64, Vec<Value>)> {
        if self.curtype != Some(type_) {
            return Err(PyErr::new(
                ExcKind::PSTypeError,
                format!(
                    "Type mismatch: {} != {}",
                    self.curtype.map_or("None".to_string(), str_repr),
                    str_repr(type_)
                ),
            ));
        }
        let objs: Vec<Value> = self.curstack.drain(..).map(|(_, obj)| obj).collect();
        let (pos, curtype, curstack) = self.context.pop().expect("context");
        self.curtype = curtype;
        self.curstack = curstack;
        debug!("end_type: pos={pos:?}, type={type_:?}, objs={objs:?}");
        Ok((pos, objs))
    }
}

/// Builds the dict of a `<< ... >>` (or inline image) from its flat key/value list:
/// `{literal_name(k): v for (k, v) in choplist(2, objs) if v is not None}`.
pub fn dict_from_pairs(objs: Vec<Value>, skip_none: bool) -> PyResult<Dict> {
    let mut d = Dict::new();
    for pair in choplist(2, objs) {
        let mut it = pair.into_iter();
        let (k, v) = (it.next().unwrap(), it.next().unwrap());
        if skip_none && v.is_none() {
            continue;
        }
        d.insert(Rc::from(literal_name(&k)?), v);
    }
    Ok(d)
}

/// The overridable methods of the parser classes, and the algorithms that use them.
pub trait PSStackParserImpl {
    fn ps(&self) -> &PSStackParser;
    fn ps_mut(&mut self) -> &mut PSStackParser;

    /// `seek(pos)` (overridden by `PDFContentParser`).
    fn seek(&mut self, pos: i64) -> PyResult<()> {
        self.ps_mut().stack_seek(pos)
    }

    /// `fillbuf()` (overridden by `PDFContentParser`).
    fn fillbuf(&mut self) -> PyResult<bool> {
        self.ps_mut().base.base_fillbuf()
    }

    /// `flush()` (overridden by `PDFStreamParser`, `PDFContentParser`).
    fn flush(&mut self) -> PyResult<()> {
        Ok(())
    }

    /// `do_keyword(pos, token)` (overridden by every concrete parser).
    fn do_keyword(&mut self, _pos: i64, _token: PSKeyword) -> PyResult<()> {
        Ok(())
    }

    /// `nextline()`: fetches a next line that ends either with `\r` or `\n`.
    fn nextline(&mut self) -> PyResult<(i64, Vec<u8>)> {
        let mut linebuf: Vec<u8> = Vec::new();
        let linepos = {
            let b = &self.ps().base;
            b.bufpos + b.charpos as i64
        };
        let mut eol = false;
        loop {
            self.fillbuf()?;
            let b = &mut self.ps_mut().base;
            if eol {
                let c = b.buf.get(b.charpos).copied();
                // handle b'\r\n'
                if c == Some(b'\n') {
                    linebuf.push(b'\n');
                    b.charpos += 1;
                }
                break;
            }
            match search(&b.buf, b.charpos, is_eol) {
                Some(m) => {
                    let end = m + 1;
                    linebuf.extend_from_slice(&b.buf[b.charpos..end]);
                    b.charpos = end;
                    if linebuf.last() == Some(&b'\r') {
                        eol = true;
                    } else {
                        break;
                    }
                }
                None => {
                    linebuf.extend_from_slice(&b.buf[b.charpos..]);
                    b.charpos = b.buf.len();
                }
            }
        }
        debug!("nextline: {linepos:?}, {}", bytes_repr(&linebuf));
        Ok((linepos, linebuf))
    }

    /// `nexttoken()`.
    fn nexttoken(&mut self) -> PyResult<(i64, Value)> {
        if self.ps().base.eof {
            // It's not really unexpected, come on now...
            return Err(PyErr::new(ExcKind::PSEOF, "Unexpected EOF"));
        }
        while self.ps().base.tokens.is_empty() {
            let r: PyResult<()> = (|| {
                let changed_stream = self.fillbuf()?;
                let b = &mut self.ps_mut().base;
                if changed_stream && !b.curtoken.is_empty() {
                    // Fixes #1157: if the stream is changed in the middle of a token,
                    // try to parse it by tacking on whitespace.
                    b.parse1(b"\n", 0)?;
                } else {
                    let buf = std::mem::take(&mut b.buf);
                    let r = b.parse1(&buf, b.charpos);
                    b.buf = buf;
                    b.charpos = r?;
                }
                Ok(())
            })();
            match r {
                Ok(()) => {}
                Err(e) if e.is(ExcKind::PSEOF) => {
                    // If we hit EOF in the middle of a token, try to parse
                    // it by tacking on whitespace, and delay raising PSEOF
                    // until next time around
                    let b = &mut self.ps_mut().base;
                    b.charpos = b.parse1(b"\n", 0)?;
                    b.eof = true;
                    // Oh, so there wasn't actually a token there? OK.
                    if b.tokens.is_empty() {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        let token = self.ps_mut().base.tokens.pop_front().unwrap();
        debug!("nexttoken: {token:?}");
        Ok(token)
    }

    /// `nextobject()`: keywords, literals, strings, numbers, arrays and dictionaries.
    fn nextobject(&mut self) -> PyResult<PSStackEntry> {
        while self.ps().results.is_empty() {
            let (pos, token) = self.nexttoken()?;
            match &token {
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Str(_) | Value::Bytes(_) | Value::Literal(_) => {
                    // normal token
                    self.ps_mut().push((pos, token));
                }
                Value::Keyword(kw) if kw.is(KEYWORD_ARRAY_BEGIN) => {
                    // begin array
                    self.ps_mut().start_type(pos, "a");
                }
                Value::Keyword(kw) if kw.is(KEYWORD_ARRAY_END) => {
                    // end array
                    match self.ps_mut().end_type("a") {
                        Ok((p, objs)) => self.ps_mut().push((p, Value::list(objs))),
                        Err(e) if e.is(ExcKind::PSTypeError) => {
                            if settings::STRICT {
                                return Err(e);
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                Value::Keyword(kw) if kw.is(KEYWORD_DICT_BEGIN) => {
                    // begin dictionary
                    self.ps_mut().start_type(pos, "d");
                }
                Value::Keyword(kw) if kw.is(KEYWORD_DICT_END) => {
                    // end dictionary
                    let r: PyResult<()> = (|| {
                        let (p, objs) = self.ps_mut().end_type("d")?;
                        if objs.len() % 2 != 0 {
                            let error_msg = format!("Invalid dictionary construct: {:?}", Value::list(objs));
                            return Err(PyErr::new(ExcKind::PSSyntaxError, error_msg));
                        }
                        let d = dict_from_pairs(objs, true)?;
                        self.ps_mut().push((p, Value::dict(d)));
                        Ok(())
                    })();
                    match r {
                        Ok(()) => {}
                        Err(e) if e.is(ExcKind::PSTypeError) => {
                            if settings::STRICT {
                                return Err(e);
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                Value::Keyword(kw) if kw.is(KEYWORD_PROC_BEGIN) => {
                    // begin proc
                    self.ps_mut().start_type(pos, "p");
                }
                Value::Keyword(kw) if kw.is(KEYWORD_PROC_END) => {
                    // end proc
                    match self.ps_mut().end_type("p") {
                        Ok((p, objs)) => self.ps_mut().push((p, Value::list(objs))),
                        Err(e) if e.is(ExcKind::PSTypeError) => {
                            if settings::STRICT {
                                return Err(e);
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                Value::Keyword(kw) => {
                    debug!("do_keyword: pos={pos:?}, token={token:?}, stack={:?}", self.ps().curstack);
                    let kw = kw.clone();
                    self.do_keyword(pos, kw)?;
                }
                _ => {
                    // The tokenizer only produces the types above.
                    unreachable!("unknown token: pos={pos:?}, token={token:?}");
                }
            }
            if !self.ps().context.is_empty() {
                continue;
            } else {
                self.flush()?;
            }
        }
        let obj = self.ps_mut().results.pop_front().unwrap();
        debug!("nextobject: {obj:?}");
        Ok(obj)
    }
}

/// `revreadlines()`: the lines of the file from the end backwards (each with its leading line
/// break), as the generator yields them.
pub struct RevReadLines {
    data: Rc<[u8]>,
    pos: usize,
    buf: Vec<u8>,
    s: Vec<u8>,
    in_chunk: bool,
}

impl RevReadLines {
    pub fn new(fp: &BytesIO) -> Self {
        RevReadLines { data: fp.data().clone(), pos: fp.len(), buf: Vec::new(), s: Vec::new(), in_chunk: false }
    }
}

impl Iterator for RevReadLines {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            if !self.in_chunk {
                if self.pos == 0 {
                    return None;
                }
                let prevpos = self.pos;
                self.pos = self.pos.saturating_sub(BUFSIZ);
                self.s = self.data[self.pos..prevpos].to_vec();
                if self.s.is_empty() {
                    return None;
                }
                self.in_chunk = true;
            }
            let n = self.s.iter().rposition(|&c| c == b'\r' || c == b'\n');
            match n {
                None => {
                    let mut nb = std::mem::take(&mut self.s);
                    nb.extend_from_slice(&self.buf);
                    self.buf = nb;
                    self.in_chunk = false;
                }
                Some(n) => {
                    let mut line = self.s[n..].to_vec();
                    line.extend_from_slice(&self.buf);
                    self.s.truncate(n);
                    self.buf.clear();
                    return Some(line);
                }
            }
        }
    }
}

/// A plain `PSStackParser` (no keyword handling), as the tests use it.
pub struct PlainPSStackParser(pub PSStackParser);

impl PSStackParserImpl for PlainPSStackParser {
    fn ps(&self) -> &PSStackParser {
        &self.0
    }
    fn ps_mut(&mut self) -> &mut PSStackParser {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    // Port of tests/test_pdfminer_psparser.py (TestPSBaseParser).
    use super::*;

    const TESTDATA: &[u8] = br#"%!PS
begin end
 "  @ #
/a/BCD /Some_Name /foo#5f#xbaa
0 +1 -2 .5 1.234
(abc) () (abc ( def ) ghi)
(def\040\0\0404ghi) (bach\\slask) (foo\nbaa)
(this % is not a comment.)
(foo
baa)
(foo\
baa)
<> <20> < 40 4020 >
<abcd00
12345>
func/a/b{(c)do*}def
[ 1 (z) ! ]
<< /foo (bar) >>
"#;

    /// `class MyParser(PSStackParser): def flush(self): self.add_results(*self.popall())`
    struct MyParser(PSStackParser);

    impl PSStackParserImpl for MyParser {
        fn ps(&self) -> &PSStackParser {
            &self.0
        }
        fn ps_mut(&mut self) -> &mut PSStackParser {
            &mut self.0
        }
        fn flush(&mut self) -> PyResult<()> {
            let objs = self.0.popall();
            self.0.add_results(objs);
            Ok(())
        }
    }

    fn parser(s: &[u8]) -> MyParser {
        MyParser(PSStackParser::new(Some(BytesIO::from_vec(s.to_vec()))))
    }

    fn reprs(v: Vec<(i64, Value)>) -> Vec<(i64, String)> {
        v.into_iter().map(|(p, v)| (p, v.repr())).collect()
    }

    fn expected(v: &[(i64, &str)]) -> Vec<(i64, String)> {
        v.iter().map(|(p, s)| (*p, s.to_string())).collect()
    }

    const TOKENS: &[(i64, &str)] = &[
        (5, "/b'begin'"), (11, "/b'end'"), (16, "/b'\"'"), (19, "/b'@'"), (21, "/b'#'"),
        (23, "/'a'"), (25, "/'BCD'"), (30, "/'Some_Name'"), (41, "/'foo_xbaa'"), (54, "0"),
        (56, "1"), (59, "-2"), (62, "0.5"), (65, "1.234"), (71, "b'abc'"), (77, "b''"),
        (80, "b'abc ( def ) ghi'"), (98, "b'def \\x00 4ghi'"), (118, "b'bach\\\\slask'"),
        (132, "b'foo\\nbaa'"), (143, "b'this % is not a comment.'"), (170, "b'foo\\nbaa'"),
        (180, "b'foobaa'"), (191, "b''"), (194, "b' '"), (199, "b'@@ '"),
        (211, "b'\\xab\\xcd\\x00\\x124\\x05'"), (226, "/b'func'"), (230, "/'a'"), (232, "/'b'"),
        (234, "/b'{'"), (235, "b'c'"), (238, "/b'do*'"), (241, "/b'}'"), (242, "/b'def'"),
        (246, "/b'['"), (248, "1"), (250, "b'z'"), (254, "/b'!'"), (256, "/b']'"),
        (258, "/b'<<'"), (261, "/'foo'"), (266, "b'bar'"), (272, "/b'>>'"),
    ];

    const OBJS: &[(i64, &str)] = &[
        (23, "/'a'"), (25, "/'BCD'"), (30, "/'Some_Name'"), (41, "/'foo_xbaa'"), (54, "0"),
        (56, "1"), (59, "-2"), (62, "0.5"), (65, "1.234"), (71, "b'abc'"), (77, "b''"),
        (80, "b'abc ( def ) ghi'"), (98, "b'def \\x00 4ghi'"), (118, "b'bach\\\\slask'"),
        (132, "b'foo\\nbaa'"), (143, "b'this % is not a comment.'"), (170, "b'foo\\nbaa'"),
        (180, "b'foobaa'"), (191, "b''"), (194, "b' '"), (199, "b'@@ '"),
        (211, "b'\\xab\\xcd\\x00\\x124\\x05'"), (230, "/'a'"), (232, "/'b'"), (234, "[b'c']"),
        (246, "[1, b'z']"), (258, "{'foo': b'bar'}"),
    ];

    #[test]
    fn test_1() {
        let mut p = parser(TESTDATA);
        let mut r = vec![];
        loop {
            match p.nexttoken() {
                Ok(t) => r.push(t),
                Err(e) if e.is(ExcKind::PSEOF) => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(reprs(r), expected(TOKENS));
    }

    #[test]
    fn test_2() {
        let mut p = parser(TESTDATA);
        let mut r = vec![];
        loop {
            match p.nextobject() {
                Ok(t) => r.push(t),
                Err(e) if e.is(ExcKind::PSEOF) => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(reprs(r), expected(OBJS));
    }

    /// Regression test for streams that end with a keyword (issue #884).
    #[test]
    fn test_3() {
        let mut p = parser(b"Do");
        let (_pos, token) = p.nexttoken().unwrap();
        assert_eq!(token.repr(), "/b'Do'");
    }
}
