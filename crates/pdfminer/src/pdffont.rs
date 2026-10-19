//! Port of `pdfminer/pdffont.py`.
//!
//! The font classes (`PDFFont` <- `PDFSimpleFont` <- `PDFType1Font` <- `PDFTrueTypeFont`,
//! `PDFSimpleFont` <- `PDFType3Font`, `PDFFont` <- `PDFCIDFont`) are one struct, [`PDFFont`]
//! (shared as `Rc<PDFFont>`), holding the `PDFFont` attributes and a [`PDFFontKind`] with the
//! subclass state. Python keeps many attributes untyped (`fontname`, `ascent`, `default_width`,
//! `hscale`, ...): they stay [`Value`]s and the arithmetic on them follows Python's rules, so the
//! same `TypeError`s happen.
//!
//! Constructors (the `rsrcmgr` argument of the Python classes is unused and omitted):
//! [`PDFFont::new_type1`], [`PDFFont::new_truetype`], [`PDFFont::new_type3`],
//! [`PDFFont::new_cid`], and [`PDFFont::new_base`] for the base class.

use std::collections::HashMap;
use std::rc::Rc;

use indexmap::IndexMap;
use log::{debug, warn};

use crate::casting::{safe_float, safe_rect_list};
use crate::cmapdb::{CMapBase, CMapClass, CMapDB, CMapParser, is_cmap_not_found, nunpack_wide};
use crate::encodingdb::{Cid2Unicode, EncodingDB, name2unicode};
use crate::fontmetrics::{FontMetricValue, font_metrics};
use crate::pdftypes::{
    PDFStream, dict_value, int_value, list_value, num_value, py_getitem_str, resolve_all, resolve1,
    stream_value,
};
use crate::psparser::{LIT, PSKeyword, PSStackParser, PSStackParserImpl, literal_name};
use crate::pyrt::codecs::latin1_decode;
use crate::pyrt::ctype::str_strip;
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::{bytes_repr, str_repr};
use crate::pyrt::io::BytesIO;
use crate::pyrt::pystr::PyStr;
use crate::pyrt::value::{Dict, DictRef, Value, py_add, py_cmp, py_float, py_mul, py_neg, py_sub};
use crate::settings;
use crate::utils::{Rect, choplist};

/// A key of a widths dict: Python keys them by int cid *or* by unicode string, and a float key
/// (from a float `char1` in `W`) equals the int of the same value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WidthKey {
    Int(i64),
    Str(PyStr),
    /// A float key that equals no int (its bits); it can never be looked up.
    Float(u64),
}

impl WidthKey {
    /// The dict key of a number Python computed (`char1 + i`).
    fn from_number(v: &Value) -> WidthKey {
        match v {
            Value::Float(f) => {
                if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f < -(i64::MIN as f64) {
                    WidthKey::Int(*f as i64)
                } else {
                    WidthKey::Float(f.to_bits())
                }
            }
            _ => WidthKey::Int(v.as_int().expect("a number")),
        }
    }
}

/// `FontWidthDict = dict[int | str, float]` (the values are whatever the PDF holds).
pub type FontWidthDict = IndexMap<WidthKey, Value>;

/// `(w, (vx, vy))` of `get_widths2`.
pub type Width2 = (Value, (Value, Value));

/// `(a, b) = x`, `(a, b, c) = x`, ...: unpacks any iterable like Python does.
pub(crate) fn py_unpack(x: &Value, n: usize) -> PyResult<Vec<Value>> {
    let items: Vec<Value> = match x {
        Value::List(_) | Value::Tuple(_) => x.sequence_items().unwrap(),
        Value::Bytes(b) => b.iter().map(|&c| Value::Int(c as i64)).collect(),
        Value::Str(s) => s.chars().map(|c| Value::str(&c.to_string())).collect(),
        Value::Dict(d) => d.borrow().keys().map(|k| Value::Str(k.clone())).collect(),
        Value::Stream(s) => {
            // A PDFStream iterates with __getitem__(0), which raises PDFKeyError.
            return Err(s.getitem("0").unwrap_err());
        }
        other => {
            return Err(PyErr::new(
                ExcKind::TypeError,
                format!("cannot unpack non-iterable {} object", other.type_name()),
            ));
        }
    };
    if items.len() < n {
        return Err(PyErr::new(
            ExcKind::ValueError,
            format!(
                "not enough values to unpack (expected {n}, got {})",
                items.len()
            ),
        ));
    }
    if items.len() > n {
        return Err(PyErr::new(
            ExcKind::ValueError,
            format!("too many values to unpack (expected {n})"),
        ));
    }
    Ok(items)
}

/// `range(v)` argument: an int (bools included), else `TypeError`.
fn range_arg(v: &Value) -> PyResult<i64> {
    v.as_int().ok_or_else(|| {
        PyErr::new(
            ExcKind::TypeError,
            format!(
                "'{}' object cannot be interpreted as an integer",
                v.type_name()
            ),
        )
    })
}

/// Build a mapping of character widths for horizontal writing.
pub fn get_widths(seq: &[Value]) -> PyResult<FontWidthDict> {
    let mut widths = FontWidthDict::new();
    let mut r: Vec<Value> = Vec::new();
    for v in seq {
        let v = resolve1(v)?;
        if let Value::List(l) = &v {
            if let Some(char1) = r.last().cloned() {
                let items = l.borrow().clone();
                for (i, w) in items.into_iter().enumerate() {
                    let k = py_add(&char1, &Value::Int(i as i64))?;
                    widths.insert(WidthKey::from_number(&k), w);
                }
                r.clear();
            }
        } else if v.is_number() {
            // == utils.isnumber(v)
            r.push(v);
            if r.len() == 3 {
                let (char1, char2, w) = (&r[0], &r[1], &r[2]);
                if let (Some(c1), Some(c2)) = (char1.as_int(), char2.as_int()) {
                    for i in c1..=c2 {
                        widths.insert(WidthKey::Int(i), w.clone());
                    }
                } else {
                    warn!(
                        "Skipping invalid font width specification for {} to {} because either of them is not an int",
                        char1.py_str(),
                        char2.py_str()
                    );
                }
                r.clear();
            }
        } else {
            warn!(
                "Skipping invalid font width specification for {} because it is not a number or a list",
                v.py_str()
            );
        }
    }
    Ok(widths)
}

/// Build a mapping of character widths for vertical writing.
pub fn get_widths2(seq: &[Value]) -> PyResult<IndexMap<WidthKey, Width2>> {
    let mut widths: IndexMap<WidthKey, Width2> = IndexMap::new();
    let mut r: Vec<Value> = Vec::new();
    for v in seq {
        if let Value::List(l) = v {
            if let Some(char1) = r.last().cloned() {
                let items = l.borrow().clone();
                for (i, t) in choplist(3, items).into_iter().enumerate() {
                    let mut t = t.into_iter();
                    let (w, vx, vy) = (t.next().unwrap(), t.next().unwrap(), t.next().unwrap());
                    let k = py_add(&char1, &Value::Int(i as i64))?;
                    widths.insert(WidthKey::from_number(&k), (w, (vx, vy)));
                }
                r.clear();
            }
        } else if v.is_number() {
            // == utils.isnumber(v)
            r.push(v.clone());
            if r.len() == 5 {
                let (char1, char2, w, vx, vy) = (&r[0], &r[1], &r[2], &r[3], &r[4]);
                let start = range_arg(char1)?;
                let stop = range_arg(&py_add(char2, &Value::Int(1))?)?;
                for i in start..stop {
                    widths.insert(WidthKey::Int(i), (w.clone(), (vx.clone(), vy.clone())));
                }
                r.clear();
            }
        }
    }
    Ok(widths)
}

pub struct FontMetricsDB;

impl FontMetricsDB {
    /// `FONT_METRICS[fontname]`: the descriptor dict and the widths (keyed by unicode string);
    /// `KeyError` for an unknown font.
    pub fn get_metrics(fontname: &str) -> PyResult<(DictRef, FontWidthDict)> {
        let Some((props, widths)) = font_metrics(fontname) else {
            return Err(PyErr::new(ExcKind::KeyError, str_repr(fontname)));
        };
        let mut d = Dict::new();
        for (k, v) in props.iter() {
            let v = match v {
                FontMetricValue::Str(s) => Value::str(s),
                FontMetricValue::Int(i) => Value::Int(*i),
                FontMetricValue::Float(f) => Value::Float(*f),
                FontMetricValue::FloatTuple(t) => {
                    Value::tuple(t.iter().map(|f| Value::Float(*f)).collect())
                }
            };
            d.insert(Rc::from(*k), v);
        }
        let w = widths
            .iter()
            .map(|(k, v)| (WidthKey::Str(PyStr::from(*k)), Value::Int(*v)))
            .collect();
        Ok((Rc::new(std::cell::RefCell::new(d)), w))
    }
}

/// `((_, a), (_, b)) = self.pop(2)`.
fn unpack_popped(popped: Vec<(i64, Value)>, n: usize) -> PyResult<Vec<Value>> {
    if popped.len() != n {
        return Err(PyErr::new(
            ExcKind::ValueError,
            format!(
                "not enough values to unpack (expected {n}, got {})",
                popped.len()
            ),
        ));
    }
    Ok(popped.into_iter().map(|(_, v)| v).collect())
}

/// `Type1FontHeaderParser`.
pub struct Type1FontHeaderParser {
    pub ps: PSStackParser,
    _cid2unicode: HashMap<i64, PyStr>,
}

impl Type1FontHeaderParser {
    pub fn new(data: BytesIO) -> Self {
        Type1FontHeaderParser {
            ps: PSStackParser::new(Some(data)),
            _cid2unicode: HashMap::new(),
        }
    }

    /// Parse the font encoding.
    ///
    /// The Type1 font encoding maps character codes to character names. These
    /// character names could either be standard Adobe glyph names, or
    /// character names associated with custom CharStrings for this font. A
    /// CharString is a sequence of operations that describe how the character
    /// should be drawn. Currently, this function returns '' (empty string)
    /// for character names that are associated with a CharStrings.
    ///
    /// Reference: Adobe Systems Incorporated, Adobe Type 1 Font Format
    ///
    /// :returns mapping of character identifiers (cid's) to unicode characters
    pub fn get_encoding(&mut self) -> PyResult<HashMap<i64, PyStr>> {
        loop {
            let (cid, name) = match self.nextobject() {
                Ok(x) => x,
                Err(e) if e.is(ExcKind::PSEOF) => break,
                Err(e) => return Err(e),
            };
            let Value::Str(name) = name else {
                unreachable!("only add_results produces objects")
            };
            match name2unicode(&name) {
                Ok(u) => {
                    self._cid2unicode.insert(cid, u);
                }
                Err(e) if e.is(ExcKind::KeyError) => debug!("{}", e.msg),
                Err(e) => return Err(e),
            }
        }
        Ok(self._cid2unicode.clone())
    }
}

impl PSStackParserImpl for Type1FontHeaderParser {
    fn ps(&self) -> &PSStackParser {
        &self.ps
    }
    fn ps_mut(&mut self) -> &mut PSStackParser {
        &mut self.ps
    }
    fn do_keyword(&mut self, _pos: i64, token: PSKeyword) -> PyResult<()> {
        if token.is(b"put") {
            let kv = unpack_popped(self.ps.pop(2), 2)?;
            if let (Some(key), Value::Literal(_)) = (kv[0].as_int(), &kv[1]) {
                let name = literal_name(&kv[1])?;
                self.ps.add_results(vec![(key, Value::str(&name))]);
            }
        }
        Ok(())
    }
}

pub const NIBBLES: [Option<&str>; 15] = [
    Some("0"),
    Some("1"),
    Some("2"),
    Some("3"),
    Some("4"),
    Some("5"),
    Some("6"),
    Some("7"),
    Some("8"),
    Some("9"),
    Some("."),
    Some("e"),
    Some("e-"),
    None,
    Some("-"),
];

/// Mapping of cmap names. Original cmap name is kept if not in the mapping.
/// (missing reference for why DLIdent is mapped to Identity)
pub fn identity_encoder(name: &str) -> &str {
    match name {
        "DLIdent-H" => "Identity-H",
        "DLIdent-V" => "Identity-V",
        other => other,
    }
}

/// `struct.error` for a short buffer.
fn struct_error(n: usize) -> PyErr {
    PyErr::new(
        ExcKind::StructError,
        format!("unpack requires a buffer of {n} bytes"),
    )
}

/// `fp.read(n)` checked to be exactly `n` bytes, as `struct.unpack` requires (`n < 0` reads to
/// the end, which then must be empty).
fn read_exact(fp: &mut BytesIO, n: i64) -> PyResult<Vec<u8>> {
    let want = n.max(0) as usize;
    let data = fp.read(n).to_vec();
    if data.len() != want {
        return Err(struct_error(want));
    }
    Ok(data)
}

fn be_u16(b: &[u8]) -> i64 {
    u16::from_be_bytes([b[0], b[1]]) as i64
}

fn be_i16(b: &[u8]) -> i64 {
    i16::from_be_bytes([b[0], b[1]]) as i64
}

fn be_u32(b: &[u8]) -> i64 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as i64
}

/// `struct.unpack(f">{n}H", fp.read(2 * n))`.
fn read_u16s(fp: &mut BytesIO, n: i64) -> PyResult<Vec<i64>> {
    let data = read_exact(fp, 2 * n)?;
    Ok(data.as_chunks::<2>().0.iter().map(|c| be_u16(c)).collect())
}

/// `ord(fp.read(1))`.
fn ord_read(fp: &mut BytesIO) -> PyResult<i64> {
    match fp.read(1).first() {
        Some(&b) => Ok(b as i64),
        None => Err(PyErr::new(
            ExcKind::TypeError,
            "ord() expected a character, but string of length 0 found",
        )),
    }
}

/// `getdict(data)`: a CFF DICT (operator -> operands).
pub fn getdict(data: &[u8]) -> PyResult<IndexMap<i64, Vec<Value>>> {
    let mut d: IndexMap<i64, Vec<Value>> = IndexMap::new();
    let mut fp = BytesIO::from_vec(data.to_vec());
    let mut stack: Vec<Value> = Vec::new();
    while let Some(&c) = fp.read(1).first() {
        let b0 = c as i64;
        if b0 <= 21 {
            d.insert(b0, std::mem::take(&mut stack));
            continue;
        }
        let value;
        if b0 == 30 {
            let mut s = String::new();
            let mut loop_ = true;
            while loop_ {
                let b = ord_read(&mut fp)?;
                for n in [b >> 4, b & 15] {
                    if n == 15 {
                        loop_ = false;
                    } else {
                        let Some(nibble) = NIBBLES[n as usize] else {
                            return Err(PyErr::new(ExcKind::AssertionError, ""));
                        };
                        s.push_str(nibble);
                    }
                }
            }
            value = Value::Float(py_float(&Value::str(&s))?);
        } else if (32..=246).contains(&b0) {
            value = Value::Int(b0 - 139);
        } else {
            let mut b1 = ord_read(&mut fp)?;
            if (247..=250).contains(&b0) {
                value = Value::Int(((b0 - 247) << 8) + b1 + 108);
            } else if (251..=254).contains(&b0) {
                value = Value::Int(-((b0 - 251) << 8) - b1 - 108);
            } else {
                let b2 = ord_read(&mut fp)?;
                if b1 >= 128 {
                    b1 -= 256;
                }
                if b0 == 28 {
                    value = Value::Int((b1 << 8) | b2);
                } else {
                    let h = be_u16(&read_exact(&mut fp, 2)?);
                    value = Value::Int((b1 << 24) | (b2 << 16) | h);
                }
            }
        }
        stack.push(value);
    }
    Ok(d)
}

#[rustfmt::skip]
pub const STANDARD_STRINGS: [&str; 391] = [
    ".notdef", "space", "exclam", "quotedbl", "numbersign", "dollar", "percent", "ampersand",
    "quoteright", "parenleft", "parenright", "asterisk", "plus", "comma", "hyphen", "period",
    "slash", "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
    "colon", "semicolon", "less", "equal", "greater", "question", "at", "A", "B", "C", "D", "E",
    "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T", "U", "V", "W", "X",
    "Y", "Z", "bracketleft", "backslash", "bracketright", "asciicircum", "underscore", "quoteleft",
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z", "braceleft", "bar", "braceright", "asciitilde", "exclamdown",
    "cent", "sterling", "fraction", "yen", "florin", "section", "currency", "quotesingle",
    "quotedblleft", "guillemotleft", "guilsinglleft", "guilsinglright", "fi", "fl", "endash",
    "dagger", "daggerdbl", "periodcentered", "paragraph", "bullet", "quotesinglbase",
    "quotedblbase", "quotedblright", "guillemotright", "ellipsis", "perthousand", "questiondown",
    "grave", "acute", "circumflex", "tilde", "macron", "breve", "dotaccent", "dieresis", "ring",
    "cedilla", "hungarumlaut", "ogonek", "caron", "emdash", "AE", "ordfeminine", "Lslash", "Oslash",
    "OE", "ordmasculine", "ae", "dotlessi", "lslash", "oslash", "oe", "germandbls", "onesuperior",
    "logicalnot", "mu", "trademark", "Eth", "onehalf", "plusminus", "Thorn", "onequarter", "divide",
    "brokenbar", "degree", "thorn", "threequarters", "twosuperior", "registered", "minus", "eth",
    "multiply", "threesuperior", "copyright", "Aacute", "Acircumflex", "Adieresis", "Agrave",
    "Aring", "Atilde", "Ccedilla", "Eacute", "Ecircumflex", "Edieresis", "Egrave", "Iacute",
    "Icircumflex", "Idieresis", "Igrave", "Ntilde", "Oacute", "Ocircumflex", "Odieresis", "Ograve",
    "Otilde", "Scaron", "Uacute", "Ucircumflex", "Udieresis", "Ugrave", "Yacute", "Ydieresis",
    "Zcaron", "aacute", "acircumflex", "adieresis", "agrave", "aring", "atilde", "ccedilla",
    "eacute", "ecircumflex", "edieresis", "egrave", "iacute", "icircumflex", "idieresis", "igrave",
    "ntilde", "oacute", "ocircumflex", "odieresis", "ograve", "otilde", "scaron", "uacute",
    "ucircumflex", "udieresis", "ugrave", "yacute", "ydieresis", "zcaron", "exclamsmall",
    "Hungarumlautsmall", "dollaroldstyle", "dollarsuperior", "ampersandsmall", "Acutesmall",
    "parenleftsuperior", "parenrightsuperior", "twodotenleader", "onedotenleader", "zerooldstyle",
    "oneoldstyle", "twooldstyle", "threeoldstyle", "fouroldstyle", "fiveoldstyle", "sixoldstyle",
    "sevenoldstyle", "eightoldstyle", "nineoldstyle", "commasuperior", "threequartersemdash",
    "periodsuperior", "questionsmall", "asuperior", "bsuperior", "centsuperior", "dsuperior",
    "esuperior", "isuperior", "lsuperior", "msuperior", "nsuperior", "osuperior", "rsuperior",
    "ssuperior", "tsuperior", "ff", "ffi", "ffl", "parenleftinferior", "parenrightinferior",
    "Circumflexsmall", "hyphensuperior", "Gravesmall", "Asmall", "Bsmall", "Csmall", "Dsmall",
    "Esmall", "Fsmall", "Gsmall", "Hsmall", "Ismall", "Jsmall", "Ksmall", "Lsmall", "Msmall",
    "Nsmall", "Osmall", "Psmall", "Qsmall", "Rsmall", "Ssmall", "Tsmall", "Usmall", "Vsmall",
    "Wsmall", "Xsmall", "Ysmall", "Zsmall", "colonmonetary", "onefitted", "rupiah", "Tildesmall",
    "exclamdownsmall", "centoldstyle", "Lslashsmall", "Scaronsmall", "Zcaronsmall", "Dieresissmall",
    "Brevesmall", "Caronsmall", "Dotaccentsmall", "Macronsmall", "figuredash", "hypheninferior",
    "Ogoneksmall", "Ringsmall", "Cedillasmall", "questiondownsmall", "oneeighth", "threeeighths",
    "fiveeighths", "seveneighths", "onethird", "twothirds", "zerosuperior", "foursuperior",
    "fivesuperior", "sixsuperior", "sevensuperior", "eightsuperior", "ninesuperior", "zeroinferior",
    "oneinferior", "twoinferior", "threeinferior", "fourinferior", "fiveinferior", "sixinferior",
    "seveninferior", "eightinferior", "nineinferior", "centinferior", "dollarinferior",
    "periodinferior", "commainferior", "Agravesmall", "Aacutesmall", "Acircumflexsmall",
    "Atildesmall", "Adieresissmall", "Aringsmall", "AEsmall", "Ccedillasmall", "Egravesmall",
    "Eacutesmall", "Ecircumflexsmall", "Edieresissmall", "Igravesmall", "Iacutesmall",
    "Icircumflexsmall", "Idieresissmall", "Ethsmall", "Ntildesmall", "Ogravesmall", "Oacutesmall",
    "Ocircumflexsmall", "Otildesmall", "Odieresissmall", "OEsmall", "Oslashsmall", "Ugravesmall",
    "Uacutesmall", "Ucircumflexsmall", "Udieresissmall", "Yacutesmall", "Thornsmall",
    "Ydieresissmall", "001.000", "001.001", "001.002", "001.003", "Black", "Bold", "Book", "Light",
    "Medium", "Regular", "Roman", "Semibold",
];

/// `CFFFont.INDEX`. The Python object keeps a reference to the font's file; here the file is
/// passed to [`INDEX::get`].
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Debug)]
pub struct INDEX {
    /// The offsets (an offset size of up to 255 bytes makes Python ints of any size: exact here
    /// up to 15 significant bytes, `None` beyond; see `nunpack_wide`).
    pub offsets: Vec<Option<i128>>,
    pub base: i64,
}

/// `fp.seek(pos)` / `fp.read(n)` argument: `OverflowError` beyond `Py_ssize_t`.
fn ssize(v: Option<i128>) -> PyResult<i64> {
    v.and_then(|v| i64::try_from(v).ok()).ok_or_else(|| {
        PyErr::new(
            ExcKind::OverflowError,
            "Python int too large to convert to C ssize_t",
        )
    })
}

impl INDEX {
    pub fn new(fp: &mut BytesIO) -> PyResult<INDEX> {
        let h = read_exact(fp, 3)?;
        let (count, offsize) = (be_u16(&h[..2]), h[2] as i64);
        let mut offsets = Vec::new();
        for _i in 0..count + 1 {
            offsets.push(nunpack_wide(fp.read(offsize)));
        }
        let base = fp.tell() - 1;
        fp.seek(ssize(offsets.last().unwrap().map(|o| base as i128 + o))?)?;
        Ok(INDEX { offsets, base })
    }

    pub fn repr(&self) -> String {
        format!("<INDEX: size={}>", self.len())
    }

    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `self[i]`.
    pub fn get(&self, fp: &mut BytesIO, i: usize) -> PyResult<Vec<u8>> {
        let index_error = || PyErr::new(ExcKind::IndexError, "list index out of range");
        let oi = *self.offsets.get(i).ok_or_else(index_error)?;
        fp.seek(ssize(oi.map(|o| self.base as i128 + o))?)?;
        let oj = *self.offsets.get(i + 1).ok_or_else(index_error)?;
        // DIVERGE(bigint): the difference of two offsets beyond 15 bytes is not computed.
        let n = match (oj, oi) {
            (Some(j), Some(i)) => Some(j - i),
            _ => None,
        };
        Ok(fp.read(ssize(n)?).to_vec())
    }
}

/// A CFF glyph name: one of the `STANDARD_STRINGS` (`str`) or from the String INDEX (`bytes`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CFFString {
    Str(&'static str),
    Bytes(Vec<u8>),
}

/// `(x,) = seq`.
fn unpack1(v: &[Value]) -> PyResult<Value> {
    match v.len() {
        1 => Ok(v[0].clone()),
        0 => Err(PyErr::new(
            ExcKind::ValueError,
            "not enough values to unpack (expected 1, got 0)",
        )),
        _ => Err(PyErr::new(
            ExcKind::ValueError,
            "too many values to unpack (expected 1)",
        )),
    }
}

/// `fp.seek(pos)` of a CFF DICT operand (an int, or a float that `seek` rejects).
fn seek_value(fp: &mut BytesIO, pos: &Value) -> PyResult<()> {
    match pos.as_int() {
        Some(p) => fp.seek(p),
        None => Err(PyErr::new(
            ExcKind::TypeError,
            format!(
                "'{}' object cannot be interpreted as an integer",
                pos.type_name()
            ),
        )),
    }
}

/// `CFFFont`.
pub struct CFFFont {
    pub name: String,
    pub fp: BytesIO,
    pub name_index: INDEX,
    pub dict_index: INDEX,
    pub string_index: INDEX,
    pub subr_index: INDEX,
    pub top_dict: IndexMap<i64, Vec<Value>>,
    pub charstring: INDEX,
    pub nglyphs: usize,
    pub code2gid: IndexMap<i64, i64>,
    pub gid2code: IndexMap<i64, i64>,
    pub name2gid: IndexMap<CFFString, i64>,
    pub gid2name: IndexMap<i64, CFFString>,
}

impl CFFFont {
    pub fn new(name: &str, mut fp: BytesIO) -> PyResult<CFFFont> {
        // Header
        let h = read_exact(&mut fp, 4)?;
        let hdrsize = h[2] as i64;
        fp.read(hdrsize - 4);
        // Name INDEX
        let name_index = INDEX::new(&mut fp)?;
        // Top DICT INDEX
        let dict_index = INDEX::new(&mut fp)?;
        // String INDEX
        let string_index = INDEX::new(&mut fp)?;
        // Global Subr INDEX
        let subr_index = INDEX::new(&mut fp)?;
        // Top DICT DATA
        let top_dict = getdict(&dict_index.get(&mut fp, 0)?)?;
        let zero = vec![Value::Int(0)];
        let charset_pos = unpack1(top_dict.get(&15).unwrap_or(&zero))?;
        let encoding_pos = unpack1(top_dict.get(&16).unwrap_or(&zero))?;
        let charstring_pos = unpack1(top_dict.get(&17).unwrap_or(&zero))?;
        // CharStrings
        seek_value(&mut fp, &charstring_pos)?;
        let charstring = INDEX::new(&mut fp)?;
        let nglyphs = charstring.len();
        let mut font = CFFFont {
            name: name.to_string(),
            fp,
            name_index,
            dict_index,
            string_index,
            subr_index,
            top_dict,
            charstring,
            nglyphs,
            code2gid: IndexMap::new(),
            gid2code: IndexMap::new(),
            name2gid: IndexMap::new(),
            gid2name: IndexMap::new(),
        };
        let fp = &mut font.fp;
        // Encodings
        seek_value(fp, &encoding_pos)?;
        let format = fp.read(1).to_vec();
        if format == b"\x00" {
            // Format 0
            let n = read_exact(fp, 1)?[0] as i64;
            for (code, gid) in read_exact(fp, n)?.into_iter().enumerate() {
                font.code2gid.insert(code as i64, gid as i64);
                font.gid2code.insert(gid as i64, code as i64);
            }
        } else if format == b"\x01" {
            // Format 1
            let n = read_exact(fp, 1)?[0] as i64;
            let mut code = 0;
            for _i in 0..n {
                let fl = read_exact(fp, 2)?;
                let (first, nleft) = (fl[0] as i64, fl[1] as i64);
                for gid in first..first + nleft + 1 {
                    font.code2gid.insert(code, gid);
                    font.gid2code.insert(gid, code);
                    code += 1;
                }
            }
        } else {
            return Err(PyErr::new(
                ExcKind::PDFValueError,
                format!("unsupported encoding format: {}", bytes_repr(&format)),
            ));
        }
        // Charsets
        seek_value(&mut font.fp, &charset_pos)?;
        let format = font.fp.read(1).to_vec();
        if format == b"\x00" {
            // Format 0
            let n = font.nglyphs as i64 - 1;
            let sids = read_u16s(&mut font.fp, n)?;
            for (gid, sid) in sids.into_iter().enumerate() {
                let gid = gid as i64 + 1;
                let sidname = font.getstr(sid)?;
                font.name2gid.insert(sidname.clone(), gid);
                font.gid2name.insert(gid, sidname);
            }
        } else if format == b"\x01" {
            // Format 1
            let n = read_exact(&mut font.fp, 1)?[0] as i64;
            let mut sid = 0;
            for _i in 0..n {
                let fl = read_exact(&mut font.fp, 2)?;
                let (first, nleft) = (fl[0] as i64, fl[1] as i64);
                for gid in first..first + nleft + 1 {
                    let sidname = font.getstr(sid)?;
                    font.name2gid.insert(sidname.clone(), gid);
                    font.gid2name.insert(gid, sidname);
                    sid += 1;
                }
            }
        } else if format == b"\x02" {
            // Format 2
            return Err(PyErr::new(
                ExcKind::AssertionError,
                format!("('Unhandled', {})", bytes_repr(&format)),
            ));
        } else {
            return Err(PyErr::new(
                ExcKind::PDFValueError,
                format!("unsupported charset format: {}", bytes_repr(&format)),
            ));
        }
        Ok(font)
    }

    /// This returns str for one of the STANDARD_STRINGS but bytes otherwise,
    /// and appears to be a needless source of type complexity.
    pub fn getstr(&mut self, sid: i64) -> PyResult<CFFString> {
        if sid < STANDARD_STRINGS.len() as i64 {
            return Ok(CFFString::Str(STANDARD_STRINGS[sid as usize]));
        }
        Ok(CFFString::Bytes(self.string_index.get(
            &mut self.fp,
            (sid - STANDARD_STRINGS.len() as i64) as usize,
        )?))
    }
}

/// `TrueTypeFont.CMapNotFound()`.
pub fn ttf_cmap_not_found() -> PyErr {
    PyErr::new(ExcKind::TTFCMapNotFound, "")
}

/// `isinstance(e, TrueTypeFont.CMapNotFound)` for an error of `create_unicode_map`.
pub fn is_ttf_cmap_not_found(e: &PyErr) -> bool {
    e.is(ExcKind::TTFCMapNotFound)
}

/// `TrueTypeFont`.
pub struct TrueTypeFont {
    pub name: String,
    pub fp: BytesIO,
    pub tables: IndexMap<Vec<u8>, (i64, i64)>,
    pub fonttype: Vec<u8>,
}

impl TrueTypeFont {
    pub fn new(name: &str, mut fp: BytesIO) -> TrueTypeFont {
        let fonttype = fp.read(4).to_vec();
        let mut tables = IndexMap::new();
        // `except struct.error: pass`: Do not fail if there are not enough bytes to read. Even
        // for corrupted PDFs we would like to get as much information as possible, so continue.
        if let Ok(h) = read_exact(&mut fp, 8) {
            let ntables = be_u16(&h[..2]);
            for _ in 0..ntables {
                let Ok(t) = read_exact(&mut fp, 16) else {
                    break;
                };
                let (name_bytes, offset, length) =
                    (t[..4].to_vec(), be_u32(&t[8..12]), be_u32(&t[12..16]));
                tables.insert(name_bytes, (offset, length));
            }
        }
        TrueTypeFont {
            name: name.to_string(),
            fp,
            tables,
            fonttype,
        }
    }

    /// The `FileUnicodeMap` of the font's Unicode `cmap` table.
    pub fn create_unicode_map(&mut self) -> PyResult<CMapBase> {
        let Some(&(base_offset, _length)) = self.tables.get(&b"cmap"[..]) else {
            return Err(ttf_cmap_not_found());
        };
        let fp = &mut self.fp;
        fp.seek(base_offset)?;
        let h = read_exact(fp, 4)?;
        let nsubtables = be_u16(&h[2..4]);
        let mut subtables: Vec<(i64, i64, i64)> = Vec::new();
        for _i in 0..nsubtables {
            let t = read_exact(fp, 8)?;
            subtables.push((be_u16(&t[..2]), be_u16(&t[2..4]), be_u32(&t[4..8])));
        }
        let mut char2gid: IndexMap<i64, i64> = IndexMap::new();
        // Supports subtable type 0, 2, 4, 6, 10 and 12.
        for (platform_id, encoding_id, st_offset) in subtables {
            // Skip non-Unicode cmaps.
            // https://docs.microsoft.com/en-us/typography/opentype/spec/cmap
            if !(platform_id == 0 || (platform_id == 3 && [1, 10].contains(&encoding_id))) {
                continue;
            }
            fp.seek(base_offset + st_offset)?;
            let fmttype = be_u16(&read_exact(fp, 2)?);
            match fmttype {
                0 => Self::parse_cmap_format_0(fp, &mut char2gid)?,
                2 => Self::parse_cmap_format_2(fp, &mut char2gid)?,
                4 => Self::parse_cmap_format_4(fp, &mut char2gid)?,
                6 => Self::parse_cmap_format_6(fp, &mut char2gid)?,
                10 => Self::parse_cmap_format_10(fp, &mut char2gid)?,
                12 => Self::parse_cmap_format_12(fp, &mut char2gid)?,
                _ => {
                    return Err(PyErr::new(
                        ExcKind::AssertionError,
                        format!("('Unhandled', {fmttype})"),
                    ));
                }
            }
        }
        if char2gid.is_empty() {
            return Err(ttf_cmap_not_found());
        }
        // create unicode map
        let mut unicode_map = CMapBase::new_file_unicode_map();
        for (char, gid) in char2gid {
            unicode_map.add_cid2unichr(gid, &Value::Int(char))?;
        }
        Ok(unicode_map)
    }

    /// Parse cmap subtable format 0
    fn parse_cmap_format_0(fp: &mut BytesIO, char2gid: &mut IndexMap<i64, i64>) -> PyResult<()> {
        let h = read_exact(fp, 4)?;
        debug!(
            "parse_cmap_format: fmtlen={}, fmtlang={}",
            be_u16(&h[..2]),
            be_u16(&h[2..])
        );
        for (i, g) in read_exact(fp, 256)?.into_iter().enumerate() {
            char2gid.insert(i as i64, g as i64);
        }
        Ok(())
    }

    /// Parse cmap subtable format 2
    fn parse_cmap_format_2(fp: &mut BytesIO, char2gid: &mut IndexMap<i64, i64>) -> PyResult<()> {
        let h = read_exact(fp, 4)?;
        debug!(
            "parse_cmap_format: fmtlen={}, fmtlang={}",
            be_u16(&h[..2]),
            be_u16(&h[2..])
        );
        let subheaderkeys = read_u16s(fp, 256)?;
        let mut firstbytes = vec![0i64; 8192];
        for (i, &k) in subheaderkeys.iter().enumerate() {
            firstbytes[(k / 8) as usize] = i as i64;
        }
        let nhdrs = subheaderkeys.iter().max().unwrap() / 8 + 1;
        let mut hdrs: Vec<(i64, i64, i64, i64, i64)> = Vec::new();
        for i in 0..nhdrs {
            let t = read_exact(fp, 8)?;
            let (firstcode, entcount, delta, offset) = (
                be_u16(&t[..2]),
                be_u16(&t[2..4]),
                be_i16(&t[4..6]),
                be_u16(&t[6..8]),
            );
            hdrs.push((i, firstcode, entcount, delta, fp.tell() - 2 + offset));
        }
        for (i, firstcode, entcount, delta, pos) in hdrs {
            if entcount == 0 {
                continue;
            }
            let first = firstcode + (firstbytes[i as usize] << 8);
            fp.seek(pos)?;
            for c in 0..entcount {
                let mut gid = be_u16(&read_exact(fp, 2)?);
                if gid != 0 {
                    gid += delta;
                }
                char2gid.insert(first + c, gid);
            }
        }
        Ok(())
    }

    /// Parse cmap subtable format 4
    fn parse_cmap_format_4(fp: &mut BytesIO, char2gid: &mut IndexMap<i64, i64>) -> PyResult<()> {
        let h = read_exact(fp, 4)?;
        debug!(
            "parse_cmap_format: fmtlen={}, fmtlang={}",
            be_u16(&h[..2]),
            be_u16(&h[2..])
        );
        let t = read_exact(fp, 8)?;
        let segcount = be_u16(&t[..2]) / 2;
        let ecs = read_u16s(fp, segcount)?;
        fp.read(2);
        let scs = read_u16s(fp, segcount)?;
        let idds: Vec<i64> = read_exact(fp, 2 * segcount)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| be_i16(c))
            .collect();
        let pos = fp.tell();
        let idrs = read_u16s(fp, segcount)?;
        for (((ec, sc), idd), idr) in ecs.into_iter().zip(scs).zip(idds).zip(idrs) {
            if idr != 0 {
                fp.seek(pos + idr)?;
                for c in sc..ec + 1 {
                    let b = be_u16(&read_exact(fp, 2)?);
                    char2gid.insert(c, (b + idd) & 0xFFFF);
                }
            } else {
                for c in sc..ec + 1 {
                    char2gid.insert(c, (c + idd) & 0xFFFF);
                }
            }
        }
        Ok(())
    }

    /// Parse cmap subtable format 6
    fn parse_cmap_format_6(fp: &mut BytesIO, char2gid: &mut IndexMap<i64, i64>) -> PyResult<()> {
        let h = read_exact(fp, 4)?;
        debug!(
            "parse_cmap_format: fmtlen={}, fmtlang={}",
            be_u16(&h[..2]),
            be_u16(&h[2..])
        );
        let t = read_exact(fp, 4)?;
        let (firstcode, entcount) = (be_u16(&t[..2]), be_u16(&t[2..]));
        let gids = read_u16s(fp, entcount)?;
        for i in 0..entcount {
            char2gid.insert(firstcode + i, gids[i as usize]);
        }
        Ok(())
    }

    /// Parse cmap subtable format 10
    fn parse_cmap_format_10(fp: &mut BytesIO, char2gid: &mut IndexMap<i64, i64>) -> PyResult<()> {
        let h = read_exact(fp, 10)?;
        debug!(
            "parse_cmap_format: rsv={}, fmtlen={}, fmtlang={}",
            be_u16(&h[..2]),
            be_u32(&h[2..6]),
            be_u32(&h[6..10])
        );
        let t = read_exact(fp, 8)?;
        let (startcode, numchars) = (be_u32(&t[..4]), be_u32(&t[4..]));
        let gids = read_u16s(fp, numchars)?;
        for i in 0..numchars {
            char2gid.insert(startcode + i, gids[i as usize]);
        }
        Ok(())
    }

    /// Parse cmap subtable format 12
    fn parse_cmap_format_12(fp: &mut BytesIO, char2gid: &mut IndexMap<i64, i64>) -> PyResult<()> {
        let h = read_exact(fp, 10)?;
        debug!(
            "parse_cmap_format: rsv={}, fmtlen={}, fmtlang={}",
            be_u16(&h[..2]),
            be_u32(&h[2..6]),
            be_u32(&h[6..10])
        );
        let numgroups = be_u32(&read_exact(fp, 4)?);
        for _i in 0..numgroups {
            let t = read_exact(fp, 12)?;
            let (sc, ec, mut sgid) = (be_u32(&t[..4]), be_u32(&t[4..8]), be_u32(&t[8..12]));
            for code in sc..ec + 1 {
                char2gid.insert(code, sgid);
                sgid += 1;
            }
        }
        Ok(())
    }
}

/// `PDFUnicodeNotDefined(a, cid)`.
fn unicode_not_defined(a: Option<&str>, cid: i64) -> PyErr {
    let a = a.map_or("None".to_string(), str_repr);
    PyErr::new(ExcKind::PDFUnicodeNotDefined, format!("({a}, {cid})"))
}

pub const LITERAL_STANDARD_ENCODING: &str = "StandardEncoding";
pub const LITERAL_TYPE1C: &str = "Type1C";

/// The state of `PDFSimpleFont`.
#[derive(Clone)]
pub struct SimpleFont {
    pub cid2unicode: Cid2Unicode,
    pub unicode_map: Option<Rc<CMapBase>>,
}

/// `PDFType1Font` / `PDFTrueTypeFont` state.
#[derive(Clone)]
pub struct Type1Font {
    pub simple: SimpleFont,
    pub basefont: String,
    pub fontfile: Option<Rc<PDFStream>>,
}

/// `PDFType3Font` state.
#[derive(Clone)]
pub struct Type3Font {
    pub simple: SimpleFont,
    pub matrix: Vec<Value>,
}

/// `PDFCIDFont` state.
#[derive(Clone)]
pub struct CIDFont {
    pub basefont: String,
    pub cidsysteminfo: DictRef,
    pub cidcoding: String,
    pub cmap: Rc<CMapBase>,
    pub fontfile: Option<Rc<PDFStream>>,
    pub unicode_map: Option<Rc<CMapBase>>,
    pub vertical: bool,
    /// `cid -> (vx, vy)` (a tuple value).
    pub disps: IndexMap<WidthKey, Value>,
    /// The int `0` or the tuple `(None, vy)`.
    pub default_disp: Value,
}

/// A replacement for the abstract `PDFFont.to_unichr` (a subclass that only overrides it, as
/// the upstream tests do).
pub type ToUnichrFn = fn(i64) -> PyResult<PyStr>;

/// The class of a [`PDFFont`] and its subclass state.
#[derive(Clone)]
pub enum PDFFontKind {
    /// The base class `PDFFont`.
    Base(Option<ToUnichrFn>),
    Type1(Type1Font),
    TrueType(Type1Font),
    Type3(Type3Font),
    CID(CIDFont),
}

/// A font (`PDFFont` and its subclasses `PDFSimpleFont`, `PDFType1Font`, `PDFTrueTypeFont`,
/// `PDFType3Font`, `PDFCIDFont`). Shared as `Rc<PDFFont>`.
#[derive(Clone)]
pub struct PDFFont {
    pub descriptor: DictRef,
    pub widths: FontWidthDict,
    pub fontname: Value,
    pub flags: i64,
    pub ascent: Value,
    pub descent: Value,
    pub italic_angle: Value,
    pub default_width: Value,
    pub leading: Value,
    pub bbox: Rect,
    pub hscale: Value,
    pub vscale: Value,
    pub kind: PDFFontKind,
}

impl std::fmt::Debug for PDFFont {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.repr())
    }
}

/// `spec.get(key)`.
fn dget(d: &DictRef, key: &str) -> Option<Value> {
    d.borrow().get(key).cloned()
}

/// `spec.get(key, default)`.
fn dget_or(d: &DictRef, key: &str, default: Value) -> Value {
    dget(d, key).unwrap_or(default)
}

/// `spec[key]` of a Python dict: `KeyError`.
fn dgetitem(d: &DictRef, key: &str) -> PyResult<Value> {
    dget(d, key).ok_or_else(|| PyErr::new(ExcKind::KeyError, str_repr(key)))
}

fn dcontains(d: &DictRef, key: &str) -> bool {
    d.borrow().contains_key(key)
}

/// A number result of Python arithmetic as `f64`; a non-number (only possible with absurd font
/// matrices) raises the `TypeError` Python raises as soon as the caller multiplies it by a float.
fn value_f64(v: Value) -> PyResult<f64> {
    v.as_f64().ok_or_else(|| {
        PyErr::new(
            ExcKind::TypeError,
            format!(
                "unsupported operand type(s) for *: '{}' and 'float'",
                v.type_name()
            ),
        )
    })
}

/// `x.decode("latin1")`: `AttributeError` for anything but bytes.
fn decode_latin1(x: &Value) -> PyResult<String> {
    match x {
        Value::Bytes(b) => Ok(latin1_decode(b)),
        other => Err(PyErr::new(
            ExcKind::AttributeError,
            format!("'{}' object has no attribute 'decode'", other.type_name()),
        )),
    }
}

/// `data[:n]` with Python's slice rules.
fn slice_to(data: &[u8], n: i64) -> &[u8] {
    let len = data.len() as i64;
    let end = if n < 0 { (len + n).max(0) } else { n.min(len) };
    &data[..end as usize]
}

/// `apply_matrix_norm(m, (1, 1))` on the untyped `FontMatrix` items.
fn apply_matrix_norm_values(m: &[Value]) -> PyResult<(Value, Value)> {
    if m.len() != 6 {
        return Err(PyErr::new(
            ExcKind::ValueError,
            if m.len() < 6 {
                format!("not enough values to unpack (expected 6, got {})", m.len())
            } else {
                "too many values to unpack (expected 6)".to_string()
            },
        ));
    }
    let (a, b, c, d) = (&m[0], &m[1], &m[2], &m[3]);
    let one = Value::Int(1);
    let x = py_add(&py_mul(a, &one)?, &py_mul(c, &one)?)?;
    let y = py_add(&py_mul(b, &one)?, &py_mul(d, &one)?)?;
    Ok((x, y))
}

/// `sum(xs)`, as CPython 3.14's `builtin_sum_impl` computes it: ints are added exactly until the
/// first non-int; from a float on, floats and ints are added with Neumaier compensation
/// (`cs_add`); anything else goes through `+`.
fn py_sum(xs: Vec<Value>) -> PyResult<Value> {
    let mut it = xs.into_iter();
    let mut i_result: i64 = 0;
    let mut result = loop {
        let Some(item) = it.next() else {
            return Ok(Value::Int(i_result));
        };
        if let Some(b) = item.as_int()
            && let Some(r) = i_result.checked_add(b)
        {
            i_result = r;
            continue;
        }
        break py_add(&Value::Int(i_result), &item)?;
    };
    if let Value::Float(f) = result {
        let (mut hi, mut lo) = (f, 0.0f64);
        let cs_to_double = |hi: f64, lo: f64| {
            if lo != 0.0 && lo.is_finite() {
                hi + lo
            } else {
                hi
            }
        };
        loop {
            let Some(item) = it.next() else {
                return Ok(Value::Float(cs_to_double(hi, lo)));
            };
            if let Some(x) = match &item {
                Value::Float(x) => Some(*x),
                Value::Int(_) | Value::Bool(_) => item.as_f64(),
                _ => None,
            } {
                let t = hi + x;
                if hi.abs() >= x.abs() {
                    lo += (hi - t) + x;
                } else {
                    lo += (x - t) + hi;
                }
                hi = t;
                continue;
            }
            result = py_add(&Value::Float(cs_to_double(hi, lo)), &item)?;
            break;
        }
    }
    for item in it {
        result = py_add(&result, &item)?;
    }
    Ok(result)
}

impl PDFFont {
    /// `PDFFont.__init__(descriptor, widths, default_width)`; `Value::None` is the default
    /// `None` of `default_width`.
    pub fn new_base(
        descriptor: DictRef,
        widths: FontWidthDict,
        default_width: Value,
    ) -> PyResult<PDFFont> {
        Self::pdffont_init(descriptor, widths, default_width, PDFFontKind::Base(None))
    }

    /// A `PDFFont` subclass that only overrides `to_unichr`.
    pub fn new_base_with_to_unichr(
        descriptor: DictRef,
        widths: FontWidthDict,
        default_width: Value,
        to_unichr: ToUnichrFn,
    ) -> PyResult<PDFFont> {
        Self::pdffont_init(
            descriptor,
            widths,
            default_width,
            PDFFontKind::Base(Some(to_unichr)),
        )
    }

    fn pdffont_init(
        descriptor: DictRef,
        widths: FontWidthDict,
        default_width: Value,
        kind: PDFFontKind,
    ) -> PyResult<PDFFont> {
        let mut widths = widths;
        for v in widths.values_mut() {
            *v = resolve_all(v)?;
        }
        let mut fontname = resolve1(&dget_or(&descriptor, "FontName", Value::str("unknown")))?;
        if let Value::Literal(_) = fontname {
            fontname = Value::str(&literal_name(&fontname)?);
        }
        let flags = int_value(&dget_or(&descriptor, "Flags", Value::Int(0)))?;
        let ascent = num_value(&dget_or(&descriptor, "Ascent", Value::Int(0)))?;
        let mut descent = num_value(&dget_or(&descriptor, "Descent", Value::Int(0)))?;
        let italic_angle = num_value(&dget_or(&descriptor, "ItalicAngle", Value::Int(0)))?;
        let default_width = if default_width.is_none() {
            num_value(&dget_or(&descriptor, "MissingWidth", Value::Int(0)))?
        } else {
            default_width
        };
        let default_width = resolve1(&default_width)?;
        let leading = num_value(&dget_or(&descriptor, "Leading", Value::Int(0)))?;
        let bbox = Self::_parse_bbox(&descriptor)?;

        // PDF RM 9.8.1 specifies /Descent should always be a negative number.
        // PScript5.dll seems to produce Descent with a positive number, but
        // text analysis will be wrong if this is taken as correct. So force
        // descent to negative.
        if py_cmp(&descent, &Value::Int(0), ">")? {
            descent = py_neg(&descent)?;
        }
        Ok(PDFFont {
            descriptor,
            widths,
            fontname,
            flags,
            ascent,
            descent,
            italic_angle,
            default_width,
            leading,
            bbox,
            hscale: Value::Float(0.001),
            vscale: Value::Float(0.001),
            kind,
        })
    }

    /// Parse FontBBox from the fonts descriptor
    fn _parse_bbox(descriptor: &DictRef) -> PyResult<Rect> {
        let font_bbox = resolve_all(&dget_or(descriptor, "FontBBox", Value::None))?;
        match safe_rect_list(&font_bbox)? {
            Some(bbox) => Ok(bbox),
            None => {
                warn!(
                    "Could not get FontBBox from font descriptor because {} cannot be parsed as 4 floats",
                    font_bbox.repr()
                );
                Ok((0.0, 0.0, 0.0, 0.0))
            }
        }
    }

    /// `PDFSimpleFont.__init__` up to the `PDFFont.__init__` call: the encoding and the
    /// ToUnicode map.
    fn simple_init(spec: &DictRef) -> PyResult<SimpleFont> {
        // Font encoding is specified either by a name of
        // built-in encoding or a dictionary that describes
        // the differences.
        let mut default_encoding = Value::Literal(LIT(LITERAL_STANDARD_ENCODING));
        if literal_name(&dget_or(spec, "Subtype", Value::None))? == "TrueType" {
            // PDF spec: TrueType fonts without Encoding default to WinAnsiEncoding
            default_encoding = Value::Literal(LIT("WinAnsiEncoding"));
        }

        let mut encoding = default_encoding.clone();
        if let Some(e) = dget(spec, "Encoding") {
            encoding = resolve1(&e)?;
        }

        let cid2unicode = if let Value::Dict(enc) = &encoding {
            let name = literal_name(&dget_or(enc, "BaseEncoding", default_encoding))?;
            let diff = list_value(&dget_or(enc, "Differences", Value::list(vec![])))?;
            EncodingDB::get_encoding(&name, Some(&diff))
        } else {
            EncodingDB::get_encoding(&literal_name(&encoding)?, None)
        };

        let mut unicode_map = None;
        if let Some(tu) = dget(spec, "ToUnicode") {
            let strm = stream_value(&tu)?;
            let mut m = CMapBase::new_file_unicode_map();
            CMapParser::new(&mut m, BytesIO::new(strm.get_data()?)).run()?;
            unicode_map = Some(Rc::new(m));
        }
        Ok(SimpleFont {
            cid2unicode,
            unicode_map,
        })
    }

    /// `PDFType1Font(rsrcmgr, spec)`.
    pub fn new_type1(spec: &DictRef) -> PyResult<PDFFont> {
        Self::type1_init(spec, false)
    }

    /// `PDFTrueTypeFont(rsrcmgr, spec)`.
    pub fn new_truetype(spec: &DictRef) -> PyResult<PDFFont> {
        Self::type1_init(spec, true)
    }

    fn type1_init(spec: &DictRef, truetype: bool) -> PyResult<PDFFont> {
        let basefont = match dget(spec, "BaseFont") {
            Some(v) => literal_name(&v)?,
            None => {
                if settings::STRICT {
                    return Err(PyErr::new(ExcKind::PDFFontError, "BaseFont is missing"));
                }
                "unknown".to_string()
            }
        };

        let (descriptor, widths) = match FontMetricsDB::get_metrics(&basefont) {
            Ok(x) => x,
            Err(_) => {
                // KeyError
                let descriptor =
                    dict_value(&dget_or(spec, "FontDescriptor", Value::dict(Dict::new())))?;
                let firstchar = int_value(&dget_or(spec, "FirstChar", Value::Int(0)))?;
                // lastchar = int_value(spec.get('LastChar', 255))
                let width_list = list_value(&dget_or(
                    spec,
                    "Widths",
                    Value::list(vec![Value::Int(0); 256]),
                ))?;
                let mut widths = FontWidthDict::new();
                for (i, w) in width_list.iter().enumerate() {
                    widths.insert(WidthKey::Int(i as i64 + firstchar), resolve1(w)?);
                }
                (descriptor, widths)
            }
        };
        let simple = Self::simple_init(spec)?;
        let t1 = Type1Font {
            simple,
            basefont,
            fontfile: None,
        };
        let kind = if truetype {
            PDFFontKind::TrueType(t1)
        } else {
            PDFFontKind::Type1(t1)
        };
        let mut font = Self::pdffont_init(descriptor.clone(), widths, Value::None, kind)?;
        if !dcontains(spec, "Encoding") && dcontains(&descriptor, "FontFile") {
            // try to recover the missing encoding info from the font file.
            let fontfile = stream_value(&dget_or(&descriptor, "FontFile", Value::None))?;
            let length1 = int_value(&fontfile.getitem("Length1")?)?;
            let data = fontfile.get_data()?;
            let data = slice_to(&data, length1);
            let mut parser = Type1FontHeaderParser::new(BytesIO::from_vec(data.to_vec()));
            let cid2unicode = parser.get_encoding()?;
            let (PDFFontKind::Type1(t1) | PDFFontKind::TrueType(t1)) = &mut font.kind else {
                unreachable!()
            };
            t1.fontfile = Some(fontfile);
            t1.simple.cid2unicode = Rc::new(cid2unicode);
        }
        Ok(font)
    }

    /// `PDFType3Font(rsrcmgr, spec)`.
    pub fn new_type3(spec: &DictRef) -> PyResult<PDFFont> {
        let firstchar = int_value(&dget_or(spec, "FirstChar", Value::Int(0)))?;
        // lastchar = int_value(spec.get('LastChar', 0))
        let width_list = list_value(&dget_or(
            spec,
            "Widths",
            Value::list(vec![Value::Int(0); 256]),
        ))?;
        let mut widths = FontWidthDict::new();
        for (i, w) in width_list.into_iter().enumerate() {
            widths.insert(WidthKey::Int(i as i64 + firstchar), w);
        }
        let descriptor = if dcontains(spec, "FontDescriptor") {
            dict_value(&dgetitem(spec, "FontDescriptor")?)?
        } else {
            let mut d = Dict::new();
            d.insert(Rc::from("Ascent"), Value::Int(0));
            d.insert(Rc::from("Descent"), Value::Int(0));
            d.insert(Rc::from("FontBBox"), dgetitem(spec, "FontBBox")?);
            Rc::new(std::cell::RefCell::new(d))
        };
        let simple = Self::simple_init(spec)?;
        let kind = PDFFontKind::Type3(Type3Font {
            simple,
            matrix: Vec::new(),
        });
        let mut font = Self::pdffont_init(descriptor, widths, Value::None, kind)?;
        let matrix = list_value(&dget_or(spec, "FontMatrix", Value::None))?;
        let (_, descent, _, ascent) = font.bbox;
        font.descent = Value::Float(descent);
        font.ascent = Value::Float(ascent);
        let (hscale, vscale) = apply_matrix_norm_values(&matrix)?;
        font.hscale = hscale;
        font.vscale = vscale;
        let PDFFontKind::Type3(t3) = &mut font.kind else {
            unreachable!()
        };
        t3.matrix = matrix;
        Ok(font)
    }

    /// `PDFCIDFont(rsrcmgr, spec, strict)`.
    pub fn new_cid(spec: &DictRef, strict: bool) -> PyResult<PDFFont> {
        let basefont = match dget(spec, "BaseFont") {
            Some(v) => literal_name(&v)?,
            None => {
                if strict {
                    return Err(PyErr::new(ExcKind::PDFFontError, "BaseFont is missing"));
                }
                "unknown".to_string()
            }
        };
        let cidsysteminfo = dict_value(&dget_or(spec, "CIDSystemInfo", Value::dict(Dict::new())))?;
        let cid_registry = decode_latin1(&resolve1(&dget_or(
            &cidsysteminfo,
            "Registry",
            Value::bytes(b"unknown"),
        ))?)?;
        let cid_ordering = decode_latin1(&resolve1(&dget_or(
            &cidsysteminfo,
            "Ordering",
            Value::bytes(b"unknown"),
        ))?)?;
        let cidcoding = format!("{}-{}", str_strip(&cid_registry), str_strip(&cid_ordering));
        let cmap = Self::get_cmap_from_spec(spec, strict)?;

        let descriptor = match dget(spec, "FontDescriptor") {
            Some(d) => dict_value(&d)?,
            None => {
                if strict {
                    return Err(PyErr::new(
                        ExcKind::PDFFontError,
                        "FontDescriptor is missing",
                    ));
                }
                Value::new_dict()
            }
        };
        let mut ttf = None;
        let mut fontfile = None;
        if dcontains(&descriptor, "FontFile2") {
            let ff = stream_value(&dget_or(&descriptor, "FontFile2", Value::None))?;
            ttf = Some(TrueTypeFont::new(&basefont, BytesIO::new(ff.get_data()?)));
            fontfile = Some(ff);
        }
        let mut unicode_map: Option<Rc<CMapBase>> = None;
        if let Some(tu) = dget(spec, "ToUnicode") {
            if let Value::Stream(_) = tu {
                let strm = stream_value(&tu)?;
                let mut m = CMapBase::new_file_unicode_map();
                CMapParser::new(&mut m, BytesIO::new(strm.get_data()?)).run()?;
                unicode_map = Some(Rc::new(m));
            } else {
                let cmap_name = literal_name(&tu)?;
                let encoding = literal_name(&dgetitem(spec, "Encoding")?)?;
                if cid_ordering.contains("Identity")
                    || cmap_name.contains("Identity")
                    || encoding.contains("Identity")
                {
                    unicode_map = Some(Rc::new(CMapBase::new(CMapClass::IdentityUnicodeMap, &[])));
                }
            }
        } else if cidcoding == "Adobe-Identity" || cidcoding == "Adobe-UCS" {
            if let Some(ttf) = ttf.as_mut() {
                match ttf.create_unicode_map() {
                    Ok(m) => unicode_map = Some(Rc::new(m)),
                    Err(e) if is_ttf_cmap_not_found(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        } else {
            match CMapDB::get_unicode_map(&cidcoding, cmap.is_vertical()) {
                Ok(m) => unicode_map = Some(m),
                Err(e) if is_cmap_not_found(&e) => {}
                Err(e) => return Err(e),
            }
        }

        let vertical = cmap.is_vertical();
        let disps;
        let default_disp;
        let widths;
        let default_width;
        if vertical {
            // writing mode: vertical
            let widths2 = get_widths2(&list_value(&dget_or(spec, "W2", Value::list(vec![])))?)?;
            disps = widths2
                .iter()
                .map(|(cid, (_, (vx, vy)))| {
                    (cid.clone(), Value::tuple(vec![vx.clone(), vy.clone()]))
                })
                .collect();
            let dw2 = resolve1(&dget_or(
                spec,
                "DW2",
                Value::list(vec![Value::Int(880), Value::Int(-1000)]),
            ))?;
            let vy_w = py_unpack(&dw2, 2)?;
            default_disp = Value::tuple(vec![Value::None, vy_w[0].clone()]);
            widths = widths2.into_iter().map(|(cid, (w, _))| (cid, w)).collect();
            default_width = vy_w[1].clone();
        } else {
            // writing mode: horizontal
            disps = IndexMap::new();
            default_disp = Value::Int(0);
            widths = get_widths(&list_value(&dget_or(spec, "W", Value::list(vec![])))?)?;
            default_width = dget_or(spec, "DW", Value::Int(1000));
        }
        let kind = PDFFontKind::CID(CIDFont {
            basefont,
            cidsysteminfo,
            cidcoding,
            cmap,
            fontfile,
            unicode_map,
            vertical,
            disps,
            default_disp,
        });
        Self::pdffont_init(descriptor, widths, default_width, kind)
    }

    /// Get cmap from font specification
    ///
    /// For certain PDFs, Encoding Type isn't mentioned as an attribute of
    /// Encoding but as an attribute of CMapName, where CMapName is an
    /// attribute of spec['Encoding'].
    /// The horizontal/vertical modes are mentioned with different name
    /// such as 'DLIdent-H/V','OneByteIdentityH/V','Identity-H/V'.
    pub fn get_cmap_from_spec(spec: &DictRef, strict: bool) -> PyResult<Rc<CMapBase>> {
        let cmap_name = Self::_get_cmap_name(spec, strict)?;

        match CMapDB::get_cmap(&cmap_name) {
            Ok(c) => Ok(c),
            Err(e) if is_cmap_not_found(&e) => {
                if strict {
                    return Err(PyErr::new(ExcKind::PDFFontError, e.msg));
                }
                Ok(Rc::new(CMapBase::new_cmap()))
            }
            Err(e) => Err(e),
        }
    }

    /// Get cmap name from font specification
    pub fn _get_cmap_name(spec: &DictRef, strict: bool) -> PyResult<String> {
        let mut cmap_name = "unknown".to_string(); // default value

        let r: PyResult<String> = (|| {
            let spec_encoding = dgetitem(spec, "Encoding")?;
            // `hasattr(spec_encoding, "name")`: literals and keywords
            if matches!(spec_encoding, Value::Literal(_) | Value::Keyword(_)) {
                literal_name(&spec_encoding)
            } else {
                literal_name(&py_getitem_str(&spec_encoding, "CMapName")?)
            }
        })();
        match r {
            Ok(n) => cmap_name = n,
            Err(e) if e.is(ExcKind::KeyError) => {
                if strict {
                    return Err(PyErr::new(ExcKind::PDFFontError, "Encoding is unspecified"));
                }
            }
            Err(e) => return Err(e),
        }

        // `if type(cmap_name) is PDFStream:` never holds: literal_name always returns a str.

        Ok(identity_encoder(&cmap_name).to_string())
    }

    /// `font.fontname` (usually a `str`, but Python keeps whatever the descriptor holds).
    pub fn fontname(&self) -> &Value {
        &self.fontname
    }

    /// `isinstance(font, PDFSimpleFont)`.
    pub fn is_simple_font(&self) -> bool {
        matches!(
            self.kind,
            PDFFontKind::Type1(_) | PDFFontKind::TrueType(_) | PDFFontKind::Type3(_)
        )
    }

    /// `isinstance(font, PDFType1Font)` (true for `PDFTrueTypeFont` too).
    pub fn is_type1_font(&self) -> bool {
        matches!(self.kind, PDFFontKind::Type1(_) | PDFFontKind::TrueType(_))
    }

    /// `isinstance(font, PDFTrueTypeFont)`.
    pub fn is_truetype_font(&self) -> bool {
        matches!(self.kind, PDFFontKind::TrueType(_))
    }

    /// `isinstance(font, PDFType3Font)`.
    pub fn is_type3_font(&self) -> bool {
        matches!(self.kind, PDFFontKind::Type3(_))
    }

    /// `isinstance(font, PDFCIDFont)`.
    pub fn is_cid_font(&self) -> bool {
        matches!(self.kind, PDFFontKind::CID(_))
    }

    /// `font.basefont` (Type1, TrueType and CID fonts).
    pub fn basefont(&self) -> Option<&str> {
        match &self.kind {
            PDFFontKind::Type1(t) | PDFFontKind::TrueType(t) => Some(&t.basefont),
            PDFFontKind::CID(c) => Some(&c.basefont),
            _ => None,
        }
    }

    fn simple(&self) -> Option<&SimpleFont> {
        match &self.kind {
            PDFFontKind::Type1(t) | PDFFontKind::TrueType(t) => Some(&t.simple),
            PDFFontKind::Type3(t) => Some(&t.simple),
            _ => None,
        }
    }

    /// `font.is_vertical()`.
    pub fn is_vertical(&self) -> bool {
        match &self.kind {
            PDFFontKind::CID(c) => c.vertical,
            _ => false,
        }
    }

    /// `font.is_multibyte()`.
    pub fn is_multibyte(&self) -> bool {
        self.is_cid_font()
    }

    /// `font.decode(bytes)`: the character ids.
    pub fn decode(&self, bytes: &[u8]) -> PyResult<Vec<i64>> {
        match &self.kind {
            PDFFontKind::CID(c) => c.cmap.decode(bytes),
            // bytearray(bytes)  # map(ord, bytes)
            _ => Ok(bytes.iter().map(|&b| b as i64).collect()),
        }
    }

    /// Ascent above the baseline, in text space units
    pub fn get_ascent(&self) -> PyResult<f64> {
        value_f64(py_mul(&self.ascent, &self.vscale)?)
    }

    /// Descent below the baseline, in text space units; always negative
    pub fn get_descent(&self) -> PyResult<f64> {
        value_f64(py_mul(&self.descent, &self.vscale)?)
    }

    /// `font.get_width()` as the Python object (an int for a Type3 font with an integer
    /// FontMatrix and default width).
    pub fn get_width_value(&self) -> PyResult<Value> {
        let mut w = Value::Float(self.bbox.2 - self.bbox.0);
        if self.bbox.2 - self.bbox.0 == 0.0 {
            w = py_neg(&self.default_width)?;
        }
        py_mul(&w, &self.hscale)
    }

    /// `font.get_width()`.
    pub fn get_width(&self) -> PyResult<f64> {
        value_f64(self.get_width_value()?)
    }

    /// `font.get_height()`.
    pub fn get_height(&self) -> PyResult<f64> {
        let mut h = Value::Float(self.bbox.3 - self.bbox.1);
        if self.bbox.3 - self.bbox.1 == 0.0 {
            h = py_sub(&self.ascent, &self.descent)?;
        }
        value_f64(py_mul(&h, &self.vscale)?)
    }

    /// `font.char_width(cid)` as the Python object.
    pub fn char_width_value(&self, cid: i64) -> PyResult<Value> {
        // Because character widths may be mapping either IDs or strings,
        // we try to lookup the character ID first, then its str equivalent.
        if let Some(cid_width) = self.widths.get(&WidthKey::Int(cid)).and_then(safe_float) {
            return py_mul(&Value::Float(cid_width), &self.hscale);
        }

        match self.to_unichr(cid) {
            Ok(str_cid) => {
                if let Some(cid_width) = self
                    .widths
                    .get(&WidthKey::Str(str_cid))
                    .and_then(safe_float)
                {
                    return py_mul(&Value::Float(cid_width), &self.hscale);
                }
            }
            Err(e) if e.is(ExcKind::PDFUnicodeNotDefined) => {}
            Err(e) => return Err(e),
        }

        py_mul(&self.default_width, &self.hscale)
    }

    /// `font.char_width(cid)`.
    pub fn char_width(&self, cid: i64) -> PyResult<f64> {
        value_f64(self.char_width_value(cid)?)
    }

    /// `font.char_disp(cid)`: a number for horizontal fonts (the int `0` by default), a tuple
    /// `(vx or None, vy)` for vertical fonts. Kept as the Python object.
    pub fn char_disp(&self, cid: i64) -> PyResult<Value> {
        match &self.kind {
            PDFFontKind::CID(c) => Ok(c
                .disps
                .get(&WidthKey::Int(cid))
                .cloned()
                .unwrap_or_else(|| c.default_disp.clone())),
            _ => Ok(Value::Int(0)),
        }
    }

    /// `font.string_width(s)` as the Python object (the int `0` for an empty string).
    pub fn string_width_value(&self, s: &[u8]) -> PyResult<Value> {
        let mut ws = Vec::new();
        for cid in self.decode(s)? {
            ws.push(self.char_width_value(cid)?);
        }
        py_sum(ws)
    }

    /// `font.string_width(s)`.
    pub fn string_width(&self, s: &[u8]) -> PyResult<f64> {
        value_f64(self.string_width_value(s)?)
    }

    /// `font.to_unichr(cid)`: raises `PDFUnicodeNotDefined` (or `NotImplementedError` for the
    /// base class) when the character has no Unicode mapping.
    pub fn to_unichr(&self, cid: i64) -> PyResult<PyStr> {
        match &self.kind {
            PDFFontKind::Base(f) => match f {
                Some(f) => f(cid),
                None => Err(PyErr::new(ExcKind::NotImplementedError, "")),
            },
            PDFFontKind::CID(c) => {
                let r = match &c.unicode_map {
                    None => Err(PyErr::new(ExcKind::PDFKeyError, cid.to_string())),
                    Some(m) => m.get_unichr(cid),
                };
                match r {
                    Err(e) if e.is(ExcKind::KeyError) => {
                        Err(unicode_not_defined(Some(&c.cidcoding), cid))
                    }
                    r => r,
                }
            }
            _ => {
                let simple = self.simple().unwrap();
                if let Some(m) = &simple.unicode_map {
                    match m.get_unichr(cid) {
                        Ok(s) => return Ok(s),
                        Err(e) if e.is(ExcKind::KeyError) => {}
                        Err(e) => return Err(e),
                    }
                }
                simple
                    .cid2unicode
                    .get(&cid)
                    .cloned()
                    .ok_or_else(|| unicode_not_defined(None, cid))
            }
        }
    }

    /// `repr(font)`.
    pub fn repr(&self) -> String {
        match &self.kind {
            PDFFontKind::Base(_) => "<PDFFont>".to_string(),
            PDFFontKind::Type1(t) => format!("<PDFType1Font: basefont={}>", str_repr(&t.basefont)),
            PDFFontKind::TrueType(t) => {
                format!("<PDFTrueTypeFont: basefont={}>", str_repr(&t.basefont))
            }
            PDFFontKind::Type3(_) => "<PDFType3Font>".to_string(),
            PDFFontKind::CID(c) => {
                format!(
                    "<PDFCIDFont: basefont={}, cidcoding={}>",
                    str_repr(&c.basefont),
                    str_repr(&c.cidcoding)
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Port of tests/test_pdffont.py. `test_pdffont_get_widths_object_ref` needs a document to
    // resolve the reference and `test_cmap_font_12` a full extraction; both run with the
    // pdfdocument / interpreter ports.
    use super::*;
    use crate::psparser::LIT;

    fn dict(items: Vec<(&str, Value)>) -> DictRef {
        let mut d = Dict::new();
        for (k, v) in items {
            d.insert(Rc::from(k), v);
        }
        Rc::new(std::cell::RefCell::new(d))
    }

    /// Test if cmap file is read from pdfminer/cmap
    ///
    /// Regression test for https://github.com/pdfminer/pdfminer.six/issues/391
    #[test]
    fn test_get_cmap() {
        let cmap_name = "UniGB-UCS2-H";
        let spec = dict(vec![("Encoding", Value::Literal(LIT(cmap_name)))]);
        let font = PDFFont::new_cid(&spec, false).unwrap();
        assert!(font.is_cid_font());

        let cmap = PDFFont::get_cmap_from_spec(&spec, false).unwrap();

        assert_eq!(cmap.attrs.get("CMapName").unwrap().py_str(), cmap_name);
        assert!(!cmap.code2cid.is_empty());
    }

    fn mock_to_unichr(cid: i64) -> PyResult<PyStr> {
        Ok(PyStr::from(cid.to_string()))
    }

    #[test]
    fn test_pdffont_char_width_defaults() {
        let cases: Vec<(&str, Vec<(WidthKey, Value)>, f64)> = vec![
            ("No widths should use default", vec![], 0.1),
            (
                "Int cid widths should be used",
                vec![(WidthKey::Int(0), Value::Float(50.0))],
                0.05,
            ),
            (
                "Str cid widths should be used",
                vec![(WidthKey::Str(PyStr::from("0")), Value::Float(200.0))],
                0.2,
            ),
            (
                "Invalid cid widths should use default",
                vec![(WidthKey::Int(0), Value::None)],
                0.1,
            ),
            (
                "Invalid cid widths should use default",
                vec![(WidthKey::Str(PyStr::from("0")), Value::None)],
                0.1,
            ),
        ];
        for (msg, widths, expected) in cases {
            let pdffont = PDFFont::new_base_with_to_unichr(
                Value::new_dict(),
                widths.into_iter().collect(),
                Value::Float(100.0),
                mock_to_unichr,
            )
            .unwrap();
            assert_eq!(pdffont.char_width(0).unwrap(), expected, "{msg}");
        }
    }

    #[test]
    fn test_pdffont_get_widths() {
        let w = get_widths(&[
            Value::Int(0),
            Value::list(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
                Value::Int(4),
            ]),
        ])
        .unwrap();
        let got: Vec<(WidthKey, String)> = w.into_iter().map(|(k, v)| (k, v.repr())).collect();
        assert_eq!(
            got,
            (0..4)
                .map(|i| (WidthKey::Int(i), (i + 1).to_string()))
                .collect::<Vec<_>>()
        );
        let w = get_widths(&[Value::Int(0), Value::Int(4), Value::Int(3)]).unwrap();
        let got: Vec<(WidthKey, String)> = w.into_iter().map(|(k, v)| (k, v.repr())).collect();
        assert_eq!(
            got,
            (0..5)
                .map(|i| (WidthKey::Int(i), "3".to_string()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn simple_fonts_from_hand_built_specs() {
        // A standard 14 font: metrics from FONT_METRICS, widths keyed by unicode string.
        let spec = dict(vec![
            ("Subtype", Value::Literal(LIT("Type1"))),
            ("BaseFont", Value::Literal(LIT("Helvetica"))),
            ("Encoding", Value::Literal(LIT("WinAnsiEncoding"))),
        ]);
        let f = PDFFont::new_type1(&spec).unwrap();
        assert_eq!(f.repr(), "<PDFType1Font: basefont='Helvetica'>");
        assert_eq!(f.fontname().repr(), "'Helvetica'");
        assert_eq!(f.char_width(65).unwrap(), 0.667);
        assert_eq!(f.to_unichr(0x80).unwrap().as_str(), Some("\u{20ac}"));
        assert!(
            f.to_unichr(0x81)
                .unwrap_err()
                .is(ExcKind::PDFUnicodeNotDefined)
        );
        assert_eq!(f.get_descent().unwrap(), -207.0 * 0.001);
        assert_eq!(f.char_disp(65).unwrap().repr(), "0");

        // Type3 without a FontMatrix: unpacking () raises ValueError.
        let spec = dict(vec![("FontBBox", Value::list(vec![Value::Int(0); 4]))]);
        assert!(
            PDFFont::new_type3(&spec)
                .unwrap_err()
                .is(ExcKind::ValueError)
        );
        // Without a FontBBox: KeyError.
        assert!(
            PDFFont::new_type3(&dict(vec![]))
                .unwrap_err()
                .is(ExcKind::KeyError)
        );
        // An integer FontMatrix keeps integer scales.
        let spec = dict(vec![
            (
                "FontBBox",
                Value::list(vec![
                    Value::Int(0),
                    Value::Int(-2),
                    Value::Int(10),
                    Value::Int(8),
                ]),
            ),
            (
                "FontMatrix",
                Value::list(vec![
                    Value::Int(1),
                    Value::Int(0),
                    Value::Int(0),
                    Value::Int(1),
                    Value::Int(0),
                    Value::Int(0),
                ]),
            ),
            ("Widths", Value::list(vec![Value::Int(5)])),
            ("FirstChar", Value::Int(65)),
        ]);
        let f = PDFFont::new_type3(&spec).unwrap();
        assert_eq!(f.char_width_value(65).unwrap().repr(), "5.0");
        assert_eq!(f.char_width_value(66).unwrap().repr(), "0");
        assert_eq!(f.get_descent().unwrap(), -2.0);
        assert_eq!(f.repr(), "<PDFType3Font>");
    }

    #[test]
    fn cid_fonts_from_hand_built_specs() {
        // Vertical CID font with W2 and DW2.
        let spec = dict(vec![
            ("BaseFont", Value::Literal(LIT("X"))),
            ("Encoding", Value::Literal(LIT("Identity-V"))),
            (
                "CIDSystemInfo",
                Value::dict({
                    let mut d = Dict::new();
                    d.insert(Rc::from("Registry"), Value::bytes(b"Adobe"));
                    d.insert(Rc::from("Ordering"), Value::bytes(b"Japan1 "));
                    d
                }),
            ),
            (
                "W2",
                Value::list(vec![
                    Value::Int(1),
                    Value::list(vec![Value::Int(-500), Value::Int(10), Value::Int(880)]),
                    Value::Int(5),
                    Value::Int(6),
                    Value::Int(-900),
                    Value::Int(20),
                    Value::Int(800),
                ]),
            ),
        ]);
        let f = PDFFont::new_cid(&spec, false).unwrap();
        assert!(f.is_vertical() && f.is_multibyte());
        assert_eq!(
            f.repr(),
            "<PDFCIDFont: basefont='X', cidcoding='Adobe-Japan1'>"
        );
        assert_eq!(f.char_disp(1).unwrap().repr(), "(10, 880)");
        assert_eq!(f.char_disp(6).unwrap().repr(), "(20, 800)");
        assert_eq!(f.char_disp(7).unwrap().repr(), "(None, 880)");
        assert_eq!(f.char_width(5).unwrap(), -0.9);
        assert_eq!(f.char_width(7).unwrap(), -1.0);
        assert_eq!(f.decode(b"\x00\x01\x00").unwrap(), vec![1]);
        assert_eq!(f.to_unichr(1).unwrap().as_str(), Some(" "));

        // A Registry that is not bytes: AttributeError.
        let spec = dict(vec![(
            "CIDSystemInfo",
            Value::dict({
                let mut d = Dict::new();
                d.insert(Rc::from("Registry"), Value::Literal(LIT("Adobe")));
                d
            }),
        )]);
        assert!(
            PDFFont::new_cid(&spec, false)
                .unwrap_err()
                .is(ExcKind::AttributeError)
        );

        // No unicode map: to_unichr raises PDFUnicodeNotDefined; an unknown cmap is an empty CMap.
        let spec = dict(vec![("Encoding", Value::Literal(LIT("NoSuchCMap")))]);
        let f = PDFFont::new_cid(&spec, false).unwrap();
        assert!(
            f.to_unichr(3)
                .unwrap_err()
                .is(ExcKind::PDFUnicodeNotDefined)
        );
        assert!(f.decode(b"abc").unwrap().is_empty());
        assert!(
            PDFFont::new_cid(&spec, true)
                .unwrap_err()
                .is(ExcKind::PDFFontError)
        );
        assert_eq!(f.char_width(3).unwrap(), 1.0);

        // A non-stream ToUnicode with "Identity" in the names: chr(cid).
        let spec = dict(vec![
            ("Encoding", Value::Literal(LIT("Identity-H"))),
            ("ToUnicode", Value::Literal(LIT("Identity-H"))),
        ]);
        let f = PDFFont::new_cid(&spec, false).unwrap();
        assert_eq!(f.to_unichr(0xD800).unwrap().to_utf16(), vec![0xD800]);
        assert!(f.to_unichr(0x110000).unwrap_err().is(ExcKind::ValueError));
    }

    #[test]
    fn python_sum_is_compensated() {
        let f = |xs: &[f64]| {
            py_sum(xs.iter().map(|&x| Value::Float(x)).collect())
                .unwrap()
                .repr()
        };
        assert_eq!(f(&[0.1, 0.2, 0.3]), "0.6");
        assert_eq!(f(&[1e16, 1.0, -1e16]), "1.0");
        assert_eq!(f(&[]), "0");
        // Ints after a float are compensated too (CPython 3.14).
        let v = py_sum(vec![Value::Float(1e16), Value::Int(1), Value::Float(-1e16)]).unwrap();
        assert_eq!(v.repr(), "1.0");
        assert_eq!(
            py_sum(vec![Value::Int(1), Value::Bool(true)])
                .unwrap()
                .repr(),
            "2"
        );
    }

    #[test]
    fn cff_getdict() {
        // 1 0 r (real 1.5) operator 17; int forms.
        let d = getdict(&[
            139, 0, 0x1e, 0x1a, 0x5f, 17, 247, 0, 28, 0xff, 0xff, 29, 0, 1, 0, 2, 5,
        ])
        .unwrap();
        assert_eq!(
            d[&0].iter().map(|v| v.repr()).collect::<Vec<_>>(),
            vec!["0"]
        );
        assert_eq!(
            d[&17].iter().map(|v| v.repr()).collect::<Vec<_>>(),
            vec!["1.5"]
        );
        assert_eq!(
            d[&5].iter().map(|v| v.repr()).collect::<Vec<_>>(),
            vec!["108", "-1", "65538"]
        );
        assert!(
            getdict(&[0x1e, 0xd0])
                .unwrap_err()
                .is(ExcKind::AssertionError)
        );
        assert!(getdict(&[0x1e]).unwrap_err().is(ExcKind::TypeError));
        assert!(
            getdict(&[29, 0, 0, 1])
                .unwrap_err()
                .is(ExcKind::StructError)
        );
    }
}
