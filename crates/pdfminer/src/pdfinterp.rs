//! Port of `pdfminer/pdfinterp.py`.
//!
//! - `PDFContentParser` is a [`PSStackParserImpl`] (it overrides `seek`, `fillbuf`, `flush` and
//!   `do_keyword`, like the Python subclass).
//! - `PDFPageInterpreter.execute` finds the operator's method with `hasattr(self, "do_" + name)`
//!   and pops as many operands as the method takes; [`op_nargs`] is that table (every `do_*`
//!   method of the class, under its Python name).
//! - The device is shared (`Rc<RefCell<dyn PDFDevice>>`); a form XObject runs in a sub-interpreter
//!   that shares the device, the resource manager and the inline image counter.
//! - Python's recursion limit: nested form XObjects recurse (`execute` -> `do_Do` ->
//!   `render_contents` -> `execute`), see [`FORM_RECURSION_LIMIT`]. DIVERGE(recursion-depth).

#![allow(non_snake_case)]

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use indexmap::IndexMap;
use log::{debug, warn};

use crate::casting::{safe_cmyk, safe_float, safe_int_obj, safe_matrix, safe_rgb};
use crate::cmapdb::{CMapBase, CMapDB, is_cmap_not_found};
use crate::pdfcolor::{PDFColorSpace, predefined, predefined_colorspace};
use crate::pdfdevice::PDFDevice;
use crate::pdffont::PDFFont;
use crate::pdfpage::PDFPage;
use crate::pdftypes::{
    LITERALS_ASCII85_DECODE, PDFStream, dict_value, in_literals, list_value, resolve1, stream_value,
};
use crate::psparser::{BUFSIZ, KWD, PSKeyword, PSStackParser, PSStackParserImpl, keyword_name_of, literal_name};
use crate::pyrt::ctype::byte_isspace;
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::{float_repr, str_repr};
use crate::pyrt::io::BytesIO;
use crate::pyrt::value::{Dict, DictRef, Value, py_add, py_eq, py_mul, py_neg};
use crate::settings;
use crate::utils::{MATRIX_IDENTITY, Matrix, PathSegment, Point, choplist, mult_matrix};

// class PDFResourceError(PDFException): ExcKind::PDFResourceError
// class PDFInterpreterError(PDFException): ExcKind::PDFInterpreterError

pub const LITERAL_PDF: &str = "PDF";
pub const LITERAL_TEXT: &str = "Text";
pub const LITERAL_FONT: &str = "Font";
pub const LITERAL_FORM: &str = "Form";
pub const LITERAL_IMAGE: &str = "Image";

fn matrix_repr(m: &Matrix) -> String {
    format!(
        "({}, {}, {}, {}, {}, {})",
        float_repr(m.0),
        float_repr(m.1),
        float_repr(m.2),
        float_repr(m.3),
        float_repr(m.4),
        float_repr(m.5)
    )
}

/// `PDFTextState`.
#[derive(Clone)]
pub struct PDFTextState {
    pub font: Option<Rc<PDFFont>>,
    pub fontsize: f64,
    pub charspace: f64,
    pub wordspace: f64,
    pub scaling: f64,
    pub leading: f64,
    /// An int (exact: only shown in the repr).
    pub render: Value,
    pub rise: f64,
    pub matrix: Matrix,
    pub linematrix: Point,
}

impl Default for PDFTextState {
    fn default() -> Self {
        Self::new()
    }
}

impl PDFTextState {
    pub fn new() -> Self {
        let mut s = PDFTextState {
            font: None,
            fontsize: 0.0,
            charspace: 0.0,
            wordspace: 0.0,
            scaling: 100.0,
            leading: 0.0,
            render: Value::Int(0),
            rise: 0.0,
            matrix: MATRIX_IDENTITY,
            linematrix: (0.0, 0.0),
        };
        s.reset();
        s
    }

    /// `repr(textstate)` (numbers as floats: the port keeps the text state in `f64`).
    pub fn repr(&self) -> String {
        format!(
            "<PDFTextState: font={}, fontsize={}, charspace={}, wordspace={}, scaling={}, leading={}, \
             render={}, rise={}, matrix={}, linematrix=({}, {})>",
            self.font.as_ref().map_or("None".to_string(), |f| f.repr()),
            float_repr(self.fontsize),
            float_repr(self.charspace),
            float_repr(self.wordspace),
            float_repr(self.scaling),
            float_repr(self.leading),
            self.render.repr(),
            float_repr(self.rise),
            matrix_repr(&self.matrix),
            float_repr(self.linematrix.0),
            float_repr(self.linematrix.1),
        )
    }

    /// `textstate.copy()`.
    pub fn copy(&self) -> Self {
        self.clone()
    }

    pub fn reset(&mut self) {
        self.matrix = MATRIX_IDENTITY;
        self.linematrix = (0.0, 0.0);
    }
}

/// A standard color: gray, RGB or CMYK (`StandardColor`).
#[derive(Clone, Debug, PartialEq)]
pub enum StandardColor {
    /// The initial color: Python's int `0` (printed `0`, not `0.0`, by the XML converter).
    /// Every operator that sets a color produces floats.
    IntGray(i64),
    Gray(f64),
    Rgb(f64, f64, f64),
    Cmyk(f64, f64, f64, f64),
}

/// `Color`: a standard color, a pattern name (colored pattern), or `(base_color, pattern_name)`
/// (uncolored pattern).
#[derive(Clone, Debug, PartialEq)]
pub enum Color {
    Standard(StandardColor),
    Pattern(String),
    UncoloredPattern(StandardColor, String),
}

impl StandardColor {
    pub fn repr(&self) -> String {
        match self {
            StandardColor::IntGray(g) => g.to_string(),
            StandardColor::Gray(g) => float_repr(*g),
            StandardColor::Rgb(r, g, b) => format!("({}, {}, {})", float_repr(*r), float_repr(*g), float_repr(*b)),
            StandardColor::Cmyk(c, m, y, k) => {
                format!("({}, {}, {}, {})", float_repr(*c), float_repr(*m), float_repr(*y), float_repr(*k))
            }
        }
    }
}

impl Color {
    /// `repr(color)`.
    pub fn repr(&self) -> String {
        match self {
            Color::Standard(c) => c.repr(),
            Color::Pattern(name) => str_repr(name),
            Color::UncoloredPattern(c, name) => format!("({}, {})", c.repr(), str_repr(name)),
        }
    }

    /// `str(color)` (what the XML converter prints).
    pub fn py_str(&self) -> String {
        match self {
            Color::Pattern(name) => name.clone(),
            other => other.repr(),
        }
    }
}

/// A `PDFGraphicState` object, shared like Python shares it.
///
/// `LTChar.graphicstate` keeps a reference to the interpreter's current graphic state, and the
/// interpreter keeps mutating that same object (`do_rg` sets `ncolor` in place) until `Q`
/// replaces it with the copy saved by `q`. So a character's `graphicstate.ncolor` is the color at
/// the end of its graphics-state block, not at the time it was drawn. Callers (FuelFwd's
/// invisible-text check) read it, so the port shares the state the same way: mutate through the
/// `RefCell`, and `copy()` makes a new `Rc`.
pub type PDFGraphicStateRef = Rc<RefCell<PDFGraphicState>>;

/// `PDFGraphicState`.
#[derive(Clone)]
pub struct PDFGraphicState {
    /// The int `0` until `w` sets a float: the XML converter prints it with `str()`.
    pub linewidth: Value,
    pub linecap: Value,
    pub linejoin: Value,
    pub miterlimit: Value,
    /// `(dash, phase)` or `None`.
    pub dash: Option<(Value, Value)>,
    pub intent: Value,
    pub flatness: Value,
    /// stroking color
    pub scolor: Color,
    pub scs: PDFColorSpace,
    /// non stroking color
    pub ncolor: Color,
    pub ncs: PDFColorSpace,
}

impl Default for PDFGraphicState {
    fn default() -> Self {
        Self::new()
    }
}

impl PDFGraphicState {
    pub fn new() -> Self {
        PDFGraphicState {
            linewidth: Value::Int(0),
            linecap: Value::None,
            linejoin: Value::None,
            miterlimit: Value::None,
            dash: None,
            intent: Value::None,
            flatness: Value::None,
            scolor: Color::Standard(StandardColor::IntGray(0)),
            scs: predefined("DeviceGray").unwrap(),
            ncolor: Color::Standard(StandardColor::IntGray(0)),
            ncs: predefined("DeviceGray").unwrap(),
        }
    }

    /// `graphicstate.copy()`: a new, separate object.
    pub fn copy(&self) -> PDFGraphicStateRef {
        Rc::new(RefCell::new(self.clone()))
    }

    /// `repr(graphicstate)`.
    pub fn repr(&self) -> String {
        format!(
            "<PDFGraphicState: linewidth={}, linecap={}, linejoin={}, miterlimit={}, dash={}, intent={}, \
             flatness={}, stroking color={}, non stroking color={}>",
            self.linewidth.repr(),
            self.linecap.repr(),
            self.linejoin.repr(),
            self.miterlimit.repr(),
            dash_repr(&self.dash),
            self.intent.repr(),
            self.flatness.repr(),
            self.scolor.repr(),
            self.ncolor.repr(),
        )
    }
}

/// `repr(graphicstate.dash)`.
pub fn dash_repr(dash: &Option<(Value, Value)>) -> String {
    match dash {
        None => "None".to_string(),
        Some((d, p)) => format!("({}, {})", d.repr(), p.repr()),
    }
}

/// Repository of shared resources.
///
/// ResourceManager facilitates reuse of shared resources
/// such as fonts and images so that large objects are not
/// allocated multiple times.
pub struct PDFResourceManager {
    pub caching: bool,
    _cached_fonts: RefCell<HashMap<i64, Rc<PDFFont>>>,
}

/// Python frames `get_font` may nest (a `Type0` font whose descendant is a `Type0` font, ...)
/// before `RecursionError`. DIVERGE(recursion-depth): Python's limit counts the caller's frames
/// too; a cyclic chain raises `RecursionError` in both.
const GET_FONT_RECURSION_LIMIT: usize = 980;

impl PDFResourceManager {
    /// `PDFResourceManager(caching=True)`.
    pub fn new(caching: bool) -> Rc<PDFResourceManager> {
        Rc::new(PDFResourceManager { caching, _cached_fonts: RefCell::new(HashMap::new()) })
    }

    pub fn get_procset(&self, procs: &[Value]) {
        // `for proc in procs: if proc is LITERAL_PDF or proc is LITERAL_TEXT: pass else: pass`
        let _ = procs;
    }

    pub fn get_cmap(&self, cmapname: &str, strict: bool) -> PyResult<Rc<CMapBase>> {
        match CMapDB::get_cmap(cmapname) {
            Ok(c) => Ok(c),
            Err(e) if is_cmap_not_found(&e) => {
                if strict {
                    return Err(e);
                }
                Ok(Rc::new(CMapBase::new_cmap()))
            }
            Err(e) => Err(e),
        }
    }

    /// `get_font(objid, spec)`: `objid` is the font's object id (`None` for a direct font dict).
    pub fn get_font(&self, objid: Option<i64>, spec: &DictRef) -> PyResult<Rc<PDFFont>> {
        self.get_font_at(objid, spec, 0)
    }

    fn get_font_at(&self, objid: Option<i64>, spec: &DictRef, depth: usize) -> PyResult<Rc<PDFFont>> {
        if depth > GET_FONT_RECURSION_LIMIT {
            return Err(PyErr::new(ExcKind::RecursionError, "maximum recursion depth exceeded"));
        }
        // `if objid and objid in self._cached_fonts`
        let key = objid.filter(|&o| o != 0);
        if let Some(k) = key
            && let Some(font) = self._cached_fonts.borrow().get(&k)
        {
            return Ok(font.clone());
        }
        debug!("get_font: create: objid={objid:?}");
        let sget = |k: &str| spec.borrow().get(k).cloned();
        if settings::STRICT {
            match sget("Type") {
                None => return Err(PyErr::new(ExcKind::KeyError, str_repr("Type"))),
                Some(Value::Literal(l)) if l.is(LITERAL_FONT) => {}
                Some(_) => return Err(PyErr::new(ExcKind::PDFFontError, "Type is not /Font")),
            }
        }
        // Create a Font object.
        let subtype = match sget("Subtype") {
            Some(v) => literal_name(&v)?,
            None => {
                if settings::STRICT {
                    return Err(PyErr::new(ExcKind::PDFFontError, "Font Subtype is not specified."));
                }
                "Type1".to_string()
            }
        };
        let font = match subtype.as_str() {
            // Type1 Font
            "Type1" | "MMType1" => Rc::new(PDFFont::new_type1(spec)?),
            // TrueType Font
            "TrueType" => Rc::new(PDFFont::new_truetype(spec)?),
            // Type3 Font
            "Type3" => Rc::new(PDFFont::new_type3(spec)?),
            // CID Font
            "CIDFontType0" | "CIDFontType2" => Rc::new(PDFFont::new_cid(spec, settings::STRICT)?),
            "Type0" => {
                // Type0 Font
                let df = sget("DescendantFonts")
                    .ok_or_else(|| PyErr::new(ExcKind::KeyError, str_repr("DescendantFonts")))?;
                let dfonts = list_value(&df)?;
                if dfonts.is_empty() {
                    return Err(PyErr::new(ExcKind::AssertionError, ""));
                }
                let mut subspec: Dict = dict_value(&dfonts[0])?.borrow().clone();
                for k in ["Encoding", "ToUnicode"] {
                    if let Some(v) = sget(k) {
                        subspec.insert(Rc::from(k), resolve1(&v)?);
                    }
                }
                let subspec = Rc::new(RefCell::new(subspec));
                self.get_font_at(None, &subspec, depth + 1)?
            }
            _ => {
                if settings::STRICT {
                    return Err(PyErr::new(
                        ExcKind::PDFFontError,
                        format!("Invalid Font spec: {}", crate::pyrt::value::dict_repr(&spec.borrow())),
                    ));
                }
                Rc::new(PDFFont::new_type1(spec)?) // this is so wrong!
            }
        };
        if let Some(k) = key
            && self.caching
        {
            self._cached_fonts.borrow_mut().insert(k, font.clone());
        }
        Ok(font)
    }
}

/// `PDFContentParser`: the content streams of a page (or form) as one stream of objects.
pub struct PDFContentParser {
    ps: PSStackParser,
    pub streams: Vec<Value>,
    pub istream: usize,
}

pub const KEYWORD_BI: &[u8] = b"BI";
pub const KEYWORD_ID: &[u8] = b"ID";
pub const KEYWORD_EI: &[u8] = b"EI";

/// `re.sub(rb"(\x0d\x0a|[\x0d\x0a])$", b"", data)`: `$` matches at the end and before a final
/// `\n`, so up to three trailing line-break bytes can go.
fn strip_trailing_eol(data: &mut Vec<u8>) {
    let n = data.len();
    let at_end = |e: usize| e == n || (e + 1 == n && data[n - 1] == b'\n');
    let mut removed: Vec<(usize, usize)> = Vec::new();
    let mut p = n.saturating_sub(3);
    while p < n {
        let c = data[p];
        let m = if c == b'\r' && p + 1 < n && data[p + 1] == b'\n' && at_end(p + 2) {
            Some(p + 2)
        } else if (c == b'\r' || c == b'\n') && at_end(p + 1) {
            Some(p + 1)
        } else {
            None
        };
        match m {
            Some(e) => {
                removed.push((p, e));
                p = e;
            }
            None => p += 1,
        }
    }
    for (s, e) in removed.into_iter().rev() {
        data.drain(s..e);
    }
}

/// `obj[0]` for the inline image's `/F` value (anything but a literal).
fn getitem0(obj: &Value) -> PyResult<Value> {
    match obj {
        Value::List(l) => {
            l.borrow().first().cloned().ok_or_else(|| PyErr::new(ExcKind::IndexError, "list index out of range"))
        }
        Value::Tuple(t) => {
            t.first().cloned().ok_or_else(|| PyErr::new(ExcKind::IndexError, "tuple index out of range"))
        }
        Value::Bytes(b) => b
            .first()
            .map(|&c| Value::Int(c as i64))
            .ok_or_else(|| PyErr::new(ExcKind::IndexError, "index out of range")),
        Value::Str(s) => s
            .chars()
            .next()
            .map(|c| Value::str(&c.to_string()))
            .ok_or_else(|| PyErr::new(ExcKind::IndexError, "string index out of range")),
        Value::Dict(_) => Err(PyErr::new(ExcKind::KeyError, "0")),
        Value::Stream(s) => Err(s.getitem("0").unwrap_err()),
        other => Err(PyErr::new(ExcKind::TypeError, format!("'{}' object is not subscriptable", other.type_name()))),
    }
}

impl PDFContentParser {
    /// `PDFContentParser(streams)`: raises `PSEOF` when there is no stream (`__init__` seeks to
    /// 0, which opens the first stream), or what decoding the first stream raises.
    pub fn new(streams: Vec<Value>) -> PyResult<Self> {
        // PSStackParser.__init__(fp=None) is safe only because we've overloaded
        // all the methods that would attempt to access self.fp without first
        // calling self.fillfp().
        let mut p = PDFContentParser { ps: PSStackParser::new(None), streams, istream: 0 };
        // PSBaseParser.__init__ calls self.seek(0): the override.
        p.seek(0)?;
        p.ps.reset();
        Ok(p)
    }

    pub fn fillfp(&mut self) -> PyResult<bool> {
        if self.ps.base.fp.is_none() {
            let strm = if self.istream < self.streams.len() {
                let strm = stream_value(&self.streams[self.istream])?;
                self.istream += 1;
                strm
            } else {
                return Err(PyErr::new(ExcKind::PSEOF, "Unexpected EOF, file truncated?"));
            };
            self.ps.base.fp = Some(BytesIO::new(strm.get_data()?));
            return Ok(true);
        }
        Ok(false)
    }

    pub fn get_inline_data(&mut self, pos: i64, target: &[u8]) -> PyResult<(i64, Vec<u8>)> {
        self.seek(pos)?;
        let mut i = 0usize;
        let mut data: Vec<u8> = Vec::new();
        while i <= target.len() {
            self.fillbuf()?;
            let b = &mut self.ps.base;
            if i != 0 {
                let c = b.buf[b.charpos];
                data.push(c);
                b.charpos += 1;
                if (target.len() <= i && byte_isspace(c)) || (i < target.len() && c == target[i]) {
                    i += 1;
                } else {
                    i = 0;
                }
            } else {
                match b.buf[b.charpos..].iter().position(|&c| c == target[0]) {
                    Some(off) => {
                        let j = b.charpos + off;
                        data.extend_from_slice(&b.buf[b.charpos..j + 1]);
                        b.charpos = j + 1;
                        i = 1;
                    }
                    None => {
                        data.extend_from_slice(&b.buf[b.charpos..]);
                        b.charpos = b.buf.len();
                    }
                }
            }
        }
        data.truncate(data.len().saturating_sub(target.len() + 1)); // strip the last part
        strip_trailing_eol(&mut data);
        Ok((pos, data))
    }

    /// The `ID` keyword: the body of the `try` in `do_keyword`.
    fn do_id(&mut self, pos: i64) -> PyResult<()> {
        let (_, objs) = self.ps.end_type("inline")?;
        if objs.len() % 2 != 0 {
            let error_msg = format!("Invalid dictionary construct: {}", Value::list(objs).repr());
            return Err(PyErr::new(ExcKind::PSTypeError, error_msg));
        }
        let mut d = Dict::new();
        for pair in choplist(2, objs) {
            let mut it = pair.into_iter();
            let (k, v) = (it.next().unwrap(), it.next().unwrap());
            let k = literal_name(&k)?;
            d.insert(Rc::from(k.as_str()), resolve1(&v)?);
        }
        let mut eos: &[u8] = b"EI";
        if let Some(filter) = d.get("F") {
            let first = match filter {
                Value::Literal(_) => filter.clone(),
                other => getitem0(other)?,
            };
            if in_literals(&first, LITERALS_ASCII85_DECODE) {
                eos = b"~>";
            }
        }
        let (pos, mut data) = self.get_inline_data(pos + b"ID ".len() as i64, eos)?;
        if eos != b"EI" {
            // it may be necessary for decoding
            data.extend_from_slice(eos);
        }
        let obj = PDFStream::new(Rc::new(RefCell::new(d)), Rc::from(data), None);
        self.ps.push((pos, Value::Stream(Rc::new(obj))));
        if eos == b"EI" {
            // otherwise it is still in the stream
            self.ps.push((pos, Value::Keyword(KWD(KEYWORD_EI))));
        }
        Ok(())
    }
}

impl PSStackParserImpl for PDFContentParser {
    fn ps(&self) -> &PSStackParser {
        &self.ps
    }
    fn ps_mut(&mut self) -> &mut PSStackParser {
        &mut self.ps
    }

    fn seek(&mut self, pos: i64) -> PyResult<()> {
        self.fillfp()?;
        self.ps.stack_seek(pos)
    }

    fn fillbuf(&mut self) -> PyResult<bool> {
        if self.ps.base.charpos < self.ps.base.buf.len() {
            return Ok(false);
        }
        let mut new_stream;
        loop {
            new_stream = self.fillfp()?;
            let b = &mut self.ps.base;
            let fp = b.fp.as_mut().expect("fillfp opened a stream");
            b.bufpos = fp.tell();
            b.buf = fp.read(BUFSIZ as i64).to_vec();
            if !b.buf.is_empty() {
                break;
            }
            b.fp = None;
        }
        self.ps.base.charpos = 0;
        Ok(new_stream)
    }

    fn flush(&mut self) -> PyResult<()> {
        let objs = self.ps.popall();
        self.ps.add_results(objs);
        Ok(())
    }

    fn do_keyword(&mut self, pos: i64, token: PSKeyword) -> PyResult<()> {
        if token.is(KEYWORD_BI) {
            // inline image within a content stream
            self.ps.start_type(pos, "inline");
        } else if token.is(KEYWORD_ID) {
            match self.do_id(pos) {
                Ok(()) => {}
                Err(e) if e.is(ExcKind::PSTypeError) => {
                    if settings::STRICT {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        } else {
            self.ps.push((pos, Value::Keyword(token)));
        }
        Ok(())
    }
}

// Types that may appear on the PDF argument stack: `PDFStackT` is any `Value`.

/// Which entries of a matrix are Python ints (the others are floats).
///
/// The port keeps matrices in `f64`, which gives the same values, but Python's CTM starts with
/// int entries (`process_page` builds `(1, 0, 0, 1, -x0, -y0)`) and int arithmetic keeps them
/// ints, and that decides what `mult_matrix` does with a form's untyped `/Matrix`: `1 * b"x"` is
/// `b"x"`, `1.0 * b"x"` raises. So the interpreter tracks which CTM entries are ints and tells
/// the device ([`PDFDevice::set_ctm_py`]).
pub type MatrixInts = [bool; 6];

/// A matrix of floats.
pub const FLOAT_MATRIX: MatrixInts = [false; 6];

/// `mult_matrix(m1, m0)` where `m1` is an untyped sequence from the PDF (a form's `/Matrix`)
/// and `m0` the CTM, whose int entries are `m0_ints`: Python's unpacking and arithmetic, with
/// their errors (`ValueError` for a wrong length, `TypeError` for a bad operand). Returns the
/// product and which of its entries are ints.
///
/// DIVERGE(matrix-sequence): when Python's product holds a sequence without raising (e.g. a
/// `/Matrix` whose `a, b` entries are both strings, times int CTM entries), its later uses fail
/// in ways the port's `f64` matrices cannot follow; the port raises `TypeError` right away.
pub fn mult_matrix_value(m1: &[Value], m0: Matrix, m0_ints: MatrixInts) -> PyResult<(Matrix, MatrixInts)> {
    if m1.len() != 6 {
        let msg = if m1.len() < 6 {
            format!("not enough values to unpack (expected 6, got {})", m1.len())
        } else {
            format!("too many values to unpack (expected 6, got {})", m1.len())
        };
        return Err(PyErr::new(ExcKind::ValueError, msg));
    }
    let m0v: Vec<Value> = [m0.0, m0.1, m0.2, m0.3, m0.4, m0.5]
        .iter()
        .zip(m0_ints)
        .map(|(&x, int)| if int { Value::Int(x as i64) } else { Value::Float(x) })
        .collect();
    let (a1, b1, c1, d1, e1, f1) = (&m1[0], &m1[1], &m1[2], &m1[3], &m1[4], &m1[5]);
    let (a0, b0, c0, d0, e0, f0) = (&m0v[0], &m0v[1], &m0v[2], &m0v[3], &m0v[4], &m0v[5]);
    let r = [
        py_add(&py_mul(a0, a1)?, &py_mul(c0, b1)?)?,
        py_add(&py_mul(b0, a1)?, &py_mul(d0, b1)?)?,
        py_add(&py_mul(a0, c1)?, &py_mul(c0, d1)?)?,
        py_add(&py_mul(b0, c1)?, &py_mul(d0, d1)?)?,
        py_add(&py_add(&py_mul(a0, e1)?, &py_mul(c0, f1)?)?, e0)?,
        py_add(&py_add(&py_mul(b0, e1)?, &py_mul(d0, f1)?)?, f0)?,
    ];
    let mut m = [0.0; 6];
    let mut ints = FLOAT_MATRIX;
    for (i, v) in r.iter().enumerate() {
        // DIVERGE(matrix-sequence)
        m[i] = v
            .as_f64()
            .ok_or_else(|| PyErr::new(ExcKind::TypeError, format!("matrix entry is a sequence: {}", v.repr())))?;
        ints[i] = v.is_int();
    }
    Ok(((m[0], m[1], m[2], m[3], m[4], m[5]), ints))
}

/// The number of operands of `do_<name>` (`func.__code__.co_argcount - 1`), for every method
/// `hasattr(self, "do_" + name)` finds; `name` has `*`, `"` and `'` already replaced.
pub fn op_nargs(name: &str) -> Option<usize> {
    Some(match name {
        "q" | "Q" | "h" | "S" | "s" | "f" | "F" | "f_a" | "B" | "B_a" | "b" | "b_a" | "n" | "W" | "W_a" | "SCN"
        | "scn" | "SC" | "sc" | "BT" | "ET" | "BX" | "EX" | "EMC" | "T_a" | "BI" | "ID" => 0,
        "w" | "J" | "j" | "M" | "ri" | "i" | "gs" | "CS" | "cs" | "G" | "g" | "sh" | "MP" | "BMC" | "Tc" | "Tw"
        | "Tz" | "TL" | "Tr" | "Ts" | "TJ" | "Tj" | "_q" | "EI" | "Do" => 1,
        "d" | "m" | "l" | "DP" | "BDC" | "Tf" | "Td" | "TD" => 2,
        "RG" | "rg" | "_w" => 3,
        "v" | "y" | "re" | "K" | "k" => 4,
        "cm" | "c" | "Tm" => 6,
        _ => return None,
    })
}

/// Python frames of one nested form XObject: `execute` -> `do_Do` -> `render_contents`.
const FRAMES_PER_FORM: usize = 3;

/// The frames below `execute` when the interpreter opens a form's content stream.
pub const FORM_CALL_DEPTH: usize = 10;

/// The deepest nesting of form XObjects before `RecursionError`. Python raises when its stack
/// exceeds 1000 frames; under `extract_pages`/`pdf2txt.py` (`process_page` at frame depth 3) a
/// chain of nested forms fails when the interpreter of form level `k` runs at depth
/// `5 + 3k` and its deepest call (opening its content stream) needs [`FORM_CALL_DEPTH`] more
/// frames: pdfminer.six handles 328 nested forms and raises at 329 (measured).
/// DIVERGE(recursion-depth): Python's depth also depends on the caller and on the content.
pub const FORM_RECURSION_LIMIT: usize = (1000 - 5 - FORM_CALL_DEPTH) / FRAMES_PER_FORM;

/// Processor for the content of a PDF page
///
/// Reference: PDF Reference, Appendix A, Operator Summary
pub struct PDFPageInterpreter {
    pub rsrcmgr: Rc<PDFResourceManager>,
    pub device: Rc<RefCell<dyn PDFDevice>>,
    /// Track stream IDs currently being executed to detect circular references
    pub stream_ids: HashSet<i64>,
    /// Track stream IDs from parent interpreters in the call stack
    pub parent_stream_ids: HashSet<i64>,
    /// DIVERGE(inline-image-id): the number of inline images drawn so far, shared with the
    /// sub-interpreters (and with any interpreter that is given the same counter, so a run over
    /// several documents can keep counting). Python names inline images `str(id(obj))`.
    pub inline_counter: Rc<Cell<u64>>,
    /// The nesting level of form XObjects (0 for a page).
    depth: usize,

    pub resources: Value,
    pub fontmap: HashMap<Rc<str>, Rc<PDFFont>>,
    pub xobjmap: IndexMap<Rc<str>, Value>,
    pub csmap: HashMap<String, PDFColorSpace>,

    /// gstack: stack for graphical states.
    pub gstack: Vec<(Matrix, PDFTextState, PDFGraphicStateRef, MatrixInts)>,
    pub ctm: Matrix,
    /// Which entries of `ctm` are Python ints.
    pub ctm_ints: MatrixInts,
    pub textstate: PDFTextState,
    pub graphicstate: PDFGraphicStateRef,
    pub curpath: Vec<PathSegment>,
    /// argstack: stack for command arguments.
    pub argstack: Vec<Value>,
}

/// Python's `seq[start:stop]` bound for an int index.
fn slice_bound(i: i64, len: usize) -> usize {
    let len = len as i64;
    let j = if i < 0 { (i + len).max(0) } else { i.min(len) };
    j as usize
}

/// `(0, 0, 1, 1)`, the bbox of an image figure (Python ints).
fn unit_rect() -> Vec<Value> {
    vec![Value::Int(0), Value::Int(0), Value::Int(1), Value::Int(1)]
}

/// `MATRIX_IDENTITY` as Python's tuple of ints.
fn identity_values() -> Vec<Value> {
    vec![Value::Int(1), Value::Int(0), Value::Int(0), Value::Int(1), Value::Int(0), Value::Int(0)]
}

impl PDFPageInterpreter {
    /// `PDFPageInterpreter(rsrcmgr, device)`.
    pub fn new(rsrcmgr: Rc<PDFResourceManager>, device: Rc<RefCell<dyn PDFDevice>>) -> Self {
        PDFPageInterpreter {
            rsrcmgr,
            device,
            stream_ids: HashSet::new(),
            parent_stream_ids: HashSet::new(),
            inline_counter: Rc::new(Cell::new(0)),
            depth: 0,
            resources: Value::None,
            fontmap: HashMap::new(),
            xobjmap: IndexMap::new(),
            csmap: HashMap::new(),
            gstack: Vec::new(),
            ctm: MATRIX_IDENTITY,
            ctm_ints: FLOAT_MATRIX,
            textstate: PDFTextState::new(),
            graphicstate: Rc::new(RefCell::new(PDFGraphicState::new())),
            curpath: Vec::new(),
            argstack: Vec::new(),
        }
    }

    pub fn dup(&self) -> PDFPageInterpreter {
        let mut interp = PDFPageInterpreter::new(self.rsrcmgr.clone(), self.device.clone());
        interp.inline_counter = self.inline_counter.clone();
        interp.depth = self.depth;
        interp
    }

    /// Create a sub-interpreter for processing nested content streams.
    ///
    /// This is used when invoking Form XObjects to prevent circular references.
    /// Unlike dup(), this method propagates the stream ID tracking from the
    /// parent interpreter, allowing detection of circular references across
    /// nested XObject invocations.
    pub fn subinterp(&self) -> PDFPageInterpreter {
        let mut interp = self.dup();
        interp.depth = self.depth + 1;
        interp.parent_stream_ids.extend(self.parent_stream_ids.iter().copied());
        interp.parent_stream_ids.extend(self.stream_ids.iter().copied());
        interp
    }

    /// Prepare the fonts and XObjects listed in the Resource attribute.
    pub fn init_resources(&mut self, resources: Value) -> PyResult<()> {
        self.resources = resources.clone();
        self.fontmap = HashMap::new();
        self.xobjmap = IndexMap::new();
        self.csmap = predefined_colorspace().into_iter().map(|(n, cs)| (n.to_string(), cs)).collect();
        if !resources.truthy() {
            return Ok(());
        }

        fn get_colorspace(spec: &Value) -> PyResult<Option<PDFColorSpace>> {
            let name = if let Value::List(l) = spec {
                let first = l
                    .borrow()
                    .first()
                    .cloned()
                    .ok_or_else(|| PyErr::new(ExcKind::IndexError, "list index out of range"))?;
                literal_name(&first)?
            } else {
                literal_name(spec)?
            };
            let items = match spec {
                Value::List(l) => Some(l.borrow().clone()),
                _ => None,
            };
            if let Some(items) = items.as_ref().filter(|i| name == "ICCBased" && i.len() >= 2) {
                Ok(Some(PDFColorSpace::new_value(&name, stream_value(&items[1])?.getitem("N")?)))
            } else if let Some(items) = items.as_ref().filter(|i| name == "DeviceN" && i.len() >= 2) {
                Ok(Some(PDFColorSpace::new(&name, list_value(&items[1])?.len() as i64)))
            } else {
                Ok(predefined(&name))
            }
        }

        let entries: Vec<(Rc<str>, Value)> =
            dict_value(&resources)?.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (k, v) in entries {
            debug!("Resource: {}: {}", str_repr(&k), v.repr());
            if &*k == "Font" {
                let fonts: Vec<(Rc<str>, Value)> =
                    dict_value(&v)?.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                for (fontid, spec) in fonts {
                    let objid = match &spec {
                        // An id beyond i64 is a font no other ref shares in practice: not cached.
                        Value::Ref(r) => r.objid(),
                        _ => None,
                    };
                    let spec = dict_value(&spec)?;
                    let font = self.rsrcmgr.get_font(objid, &spec)?;
                    self.fontmap.insert(fontid, font);
                }
            } else if &*k == "ColorSpace" {
                let spaces: Vec<(Rc<str>, Value)> =
                    dict_value(&v)?.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                for (csid, spec) in spaces {
                    if let Some(colorspace) = get_colorspace(&resolve1(&spec)?)? {
                        self.csmap.insert(csid.to_string(), colorspace);
                    }
                }
            } else if &*k == "ProcSet" {
                self.rsrcmgr.get_procset(&list_value(&v)?);
            } else if &*k == "XObject" {
                let xobjs: Vec<(Rc<str>, Value)> =
                    dict_value(&v)?.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                for (xobjid, xobjstrm) in xobjs {
                    self.xobjmap.insert(xobjid, xobjstrm);
                }
            }
        }
        Ok(())
    }

    /// Initialize the text and graphic states for rendering a page.
    pub fn init_state(&mut self, ctm: Matrix) {
        self.init_state_py(ctm, FLOAT_MATRIX);
    }

    /// `init_state(ctm)` where the entries `ctm_ints` of `ctm` are Python ints.
    pub fn init_state_py(&mut self, ctm: Matrix, ctm_ints: MatrixInts) {
        // gstack: stack for graphical states.
        self.gstack = Vec::new();
        self.ctm = ctm;
        self.ctm_ints = ctm_ints;
        self.device.borrow_mut().set_ctm_py(self.ctm, self.ctm_ints);
        self.textstate = PDFTextState::new();
        self.graphicstate = Rc::new(RefCell::new(PDFGraphicState::new()));
        self.curpath = Vec::new();
        // argstack: stack for command arguments.
        self.argstack = Vec::new();
    }

    pub fn push(&mut self, obj: Value) {
        self.argstack.push(obj);
    }

    pub fn pop(&mut self, n: usize) -> Vec<Value> {
        if n == 0 {
            return Vec::new();
        }
        let start = self.argstack.len().saturating_sub(n);
        self.argstack.split_off(start)
    }

    /// `self.pop(n)` with `n` an untyped `ncomponents` (Python's slicing rules, `TypeError` for
    /// a non-int).
    fn pop_value(&mut self, n: &Value) -> PyResult<Vec<Value>> {
        if py_eq(n, &Value::Int(0)) {
            return Ok(Vec::new());
        }
        let neg = py_neg(n)?;
        let Some(neg) = (if matches!(neg, Value::Float(_)) { None } else { neg.as_int() }) else {
            return Err(PyErr::new(
                ExcKind::TypeError,
                "slice indices must be integers or None or have an __index__ method",
            ));
        };
        let len = self.argstack.len();
        // x = self.argstack[-n:]; self.argstack = self.argstack[:-n]
        let start = slice_bound(neg, len);
        let x = self.argstack[start..].to_vec();
        self.argstack.truncate(start);
        Ok(x)
    }

    pub fn get_current_state(&self) -> (Matrix, PDFTextState, PDFGraphicStateRef, MatrixInts) {
        (self.ctm, self.textstate.copy(), self.graphicstate.borrow().copy(), self.ctm_ints)
    }

    pub fn set_current_state(&mut self, state: (Matrix, PDFTextState, PDFGraphicStateRef, MatrixInts)) {
        (self.ctm, self.textstate, self.graphicstate, self.ctm_ints) = state;
        self.device.borrow_mut().set_ctm_py(self.ctm, self.ctm_ints);
    }

    /// Save graphics state
    pub fn do_q(&mut self) {
        let s = self.get_current_state();
        self.gstack.push(s);
    }

    /// Restore graphics state
    pub fn do_Q(&mut self) {
        if let Some(s) = self.gstack.pop() {
            self.set_current_state(s);
        }
    }

    /// Concatenate matrix to current transformation matrix
    pub fn do_cm(&mut self, a1: &Value, b1: &Value, c1: &Value, d1: &Value, e1: &Value, f1: &Value) {
        match safe_matrix(a1, b1, c1, d1, e1, f1) {
            None => warn!(
                "Cannot concatenate matrix to current transformation matrix because not all values in {} can be \
                 parsed as floats",
                Value::tuple(vec![a1.clone(), b1.clone(), c1.clone(), d1.clone(), e1.clone(), f1.clone()]).repr()
            ),
            Some(matrix) => {
                // `safe_matrix` gives floats: the product is all floats.
                self.ctm = mult_matrix(matrix, self.ctm);
                self.ctm_ints = FLOAT_MATRIX;
                self.device.borrow_mut().set_ctm_py(self.ctm, self.ctm_ints);
            }
        }
    }

    /// Set line width
    pub fn do_w(&mut self, linewidth: &Value) -> PyResult<()> {
        match safe_float(linewidth) {
            None => warn!("Cannot set line width because {} is an invalid float value", linewidth.repr()),
            Some(linewidth_f) => {
                let pow = crate::pyrt::num::pow;
                let scale = pow(pow(self.ctm.0, 2.0)? + pow(self.ctm.1, 2.0)?, 0.5)?;
                self.graphicstate.borrow_mut().linewidth = Value::Float(linewidth_f * scale);
            }
        }
        Ok(())
    }

    /// Set line cap style
    pub fn do_J(&mut self, linecap: &Value) {
        self.graphicstate.borrow_mut().linecap = linecap.clone();
    }

    /// Set line join style
    pub fn do_j(&mut self, linejoin: &Value) {
        self.graphicstate.borrow_mut().linejoin = linejoin.clone();
    }

    /// Set miter limit
    pub fn do_M(&mut self, miterlimit: &Value) {
        self.graphicstate.borrow_mut().miterlimit = miterlimit.clone();
    }

    /// Set line dash pattern
    pub fn do_d(&mut self, dash: &Value, phase: &Value) {
        self.graphicstate.borrow_mut().dash = Some((dash.clone(), phase.clone()));
    }

    /// Set color rendering intent
    pub fn do_ri(&mut self, intent: &Value) {
        self.graphicstate.borrow_mut().intent = intent.clone();
    }

    /// Set flatness tolerance
    pub fn do_i(&mut self, flatness: &Value) {
        self.graphicstate.borrow_mut().flatness = flatness.clone();
    }

    /// Set parameters from graphics state parameter dictionary
    pub fn do_gs(&mut self, _name: &Value) {
        // TODO
    }

    /// Begin new subpath
    pub fn do_m(&mut self, x: &Value, y: &Value) {
        match (safe_float(x), safe_float(y)) {
            (Some(x_f), Some(y_f)) => self.curpath.push(PathSegment { op: 'm', args: vec![x_f, y_f] }),
            _ => warn!(
                "Cannot start new subpath because not all values in ('m', {}, {}) can be parsed as floats",
                x.repr(),
                y.repr()
            ),
        }
    }

    /// Append straight line segment to path
    pub fn do_l(&mut self, x: &Value, y: &Value) {
        match (safe_float(x), safe_float(y)) {
            (Some(x_f), Some(y_f)) => self.curpath.push(PathSegment { op: 'l', args: vec![x_f, y_f] }),
            _ => warn!(
                "Cannot append straight line segment to path because not all values in ('l', {}, {}) can be \
                 parsed as floats",
                x.repr(),
                y.repr()
            ),
        }
    }

    fn curve(&mut self, op: char, vals: &[&Value]) {
        let fs: Vec<Option<f64>> = vals.iter().map(|v| safe_float(v)).collect();
        if fs.iter().any(|f| f.is_none()) {
            warn!(
                "Cannot append curved segment to path because not all values in ('{op}', {}) can be parsed as \
                 floats",
                vals.iter().map(|v| v.repr()).collect::<Vec<_>>().join(", ")
            );
        } else {
            self.curpath.push(PathSegment { op, args: fs.into_iter().map(|f| f.unwrap()).collect() });
        }
    }

    /// Append curved segment to path (three control points)
    pub fn do_c(&mut self, x1: &Value, y1: &Value, x2: &Value, y2: &Value, x3: &Value, y3: &Value) {
        self.curve('c', &[x1, y1, x2, y2, x3, y3]);
    }

    /// Append curved segment to path (initial point replicated)
    pub fn do_v(&mut self, x2: &Value, y2: &Value, x3: &Value, y3: &Value) {
        self.curve('v', &[x2, y2, x3, y3]);
    }

    /// Append curved segment to path (final point replicated)
    pub fn do_y(&mut self, x1: &Value, y1: &Value, x3: &Value, y3: &Value) {
        self.curve('y', &[x1, y1, x3, y3]);
    }

    /// Close subpath
    pub fn do_h(&mut self) {
        self.curpath.push(PathSegment { op: 'h', args: vec![] });
    }

    /// Append rectangle to path
    pub fn do_re(&mut self, x: &Value, y: &Value, w: &Value, h: &Value) {
        match (safe_float(x), safe_float(y), safe_float(w), safe_float(h)) {
            (Some(x_f), Some(y_f), Some(w_f), Some(h_f)) => {
                self.curpath.push(PathSegment { op: 'm', args: vec![x_f, y_f] });
                self.curpath.push(PathSegment { op: 'l', args: vec![x_f + w_f, y_f] });
                self.curpath.push(PathSegment { op: 'l', args: vec![x_f + w_f, y_f + h_f] });
                self.curpath.push(PathSegment { op: 'l', args: vec![x_f, y_f + h_f] });
                self.curpath.push(PathSegment { op: 'h', args: vec![] });
            }
            _ => warn!(
                "Cannot append rectangle to path because not all values in ({}, {}, {}, {}) can be parsed as floats",
                x.repr(),
                y.repr(),
                w.repr(),
                h.repr()
            ),
        }
    }

    fn paint(&mut self, stroke: bool, fill: bool, evenodd: bool) -> PyResult<()> {
        let path = std::mem::take(&mut self.curpath);
        let gs = self.graphicstate.clone();
        self.device.borrow_mut().paint_path(&gs, stroke, fill, evenodd, &path)?;
        self.curpath = Vec::new();
        Ok(())
    }

    /// Stroke path
    pub fn do_S(&mut self) -> PyResult<()> {
        self.paint(true, false, false)
    }

    /// Close and stroke path
    pub fn do_s(&mut self) -> PyResult<()> {
        self.do_h();
        self.do_S()
    }

    /// Fill path using nonzero winding number rule
    pub fn do_f(&mut self) -> PyResult<()> {
        self.paint(false, true, false)
    }

    /// Fill path using nonzero winding number rule (obsolete)
    pub fn do_F(&mut self) {}

    /// Fill path using even-odd rule
    pub fn do_f_a(&mut self) -> PyResult<()> {
        self.paint(false, true, true)
    }

    /// Fill and stroke path using nonzero winding number rule
    pub fn do_B(&mut self) -> PyResult<()> {
        self.paint(true, true, false)
    }

    /// Fill and stroke path using even-odd rule
    pub fn do_B_a(&mut self) -> PyResult<()> {
        self.paint(true, true, true)
    }

    /// Close, fill, and stroke path using nonzero winding number rule
    pub fn do_b(&mut self) -> PyResult<()> {
        self.do_h();
        self.do_B()
    }

    /// Close, fill, and stroke path using even-odd rule
    pub fn do_b_a(&mut self) -> PyResult<()> {
        self.do_h();
        self.do_B_a()
    }

    /// End path without filling or stroking
    pub fn do_n(&mut self) {
        self.curpath = Vec::new();
    }

    /// Set clipping path using nonzero winding number rule
    pub fn do_W(&mut self) {}

    /// Set clipping path using even-odd rule
    pub fn do_W_a(&mut self) {}

    fn set_colorspace(&mut self, name: &Value, stroke: bool) -> PyResult<()> {
        match self.csmap.get(&literal_name(name)?) {
            Some(cs) => {
                let mut gs = self.graphicstate.borrow_mut();
                if stroke {
                    gs.scs = cs.clone();
                } else {
                    gs.ncs = cs.clone();
                }
            }
            None => {
                if settings::STRICT {
                    return Err(PyErr::new(
                        ExcKind::PDFInterpreterError,
                        format!("Undefined ColorSpace: {}", name.repr()),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Set color space for stroking operations
    ///
    /// Introduced in PDF 1.1
    pub fn do_CS(&mut self, name: &Value) -> PyResult<()> {
        self.set_colorspace(name, true)
    }

    /// Set color space for nonstroking operations
    pub fn do_cs(&mut self, name: &Value) -> PyResult<()> {
        self.set_colorspace(name, false)
    }

    /// `self.csmap[name]` for the device color spaces (`KeyError` cannot happen: the map starts
    /// as a copy of `PREDEFINED_COLORSPACE` and keys are only overwritten).
    fn device_cs(&self, name: &str) -> PDFColorSpace {
        self.csmap.get(name).cloned().expect("predefined color space")
    }

    fn set_color(&mut self, stroke: bool, color: StandardColor, csname: &str) {
        let cs = self.device_cs(csname);
        let mut gs = self.graphicstate.borrow_mut();
        if stroke {
            gs.scolor = Color::Standard(color);
            gs.scs = cs;
        } else {
            gs.ncolor = Color::Standard(color);
            gs.ncs = cs;
        }
    }

    /// Set gray level for stroking operations
    pub fn do_G(&mut self, gray: &Value) {
        match safe_float(gray) {
            None => warn!("Cannot set gray level because {} is an invalid float value", gray.repr()),
            Some(gray_f) => self.set_color(true, StandardColor::Gray(gray_f), "DeviceGray"),
        }
    }

    /// Set gray level for nonstroking operations
    pub fn do_g(&mut self, gray: &Value) {
        match safe_float(gray) {
            None => warn!("Cannot set gray level because {} is an invalid float value", gray.repr()),
            Some(gray_f) => self.set_color(false, StandardColor::Gray(gray_f), "DeviceGray"),
        }
    }

    /// Set RGB color for stroking operations
    pub fn do_RG(&mut self, r: &Value, g: &Value, b: &Value) {
        match safe_rgb(r, g, b) {
            None => warn!(
                "Cannot set RGB stroke color because not all values in ({}, {}, {}) can be parsed as floats",
                r.repr(),
                g.repr(),
                b.repr()
            ),
            Some((r, g, b)) => self.set_color(true, StandardColor::Rgb(r, g, b), "DeviceRGB"),
        }
    }

    /// Set RGB color for nonstroking operations
    pub fn do_rg(&mut self, r: &Value, g: &Value, b: &Value) {
        match safe_rgb(r, g, b) {
            None => warn!(
                "Cannot set RGB non-stroke color because not all values in ({}, {}, {}) can be parsed as floats",
                r.repr(),
                g.repr(),
                b.repr()
            ),
            Some((r, g, b)) => self.set_color(false, StandardColor::Rgb(r, g, b), "DeviceRGB"),
        }
    }

    /// Set CMYK color for stroking operations
    pub fn do_K(&mut self, c: &Value, m: &Value, y: &Value, k: &Value) {
        match safe_cmyk(c, m, y, k) {
            None => warn!(
                "Cannot set CMYK stroke color because not all values in ({}, {}, {}, {}) can be parsed as floats",
                c.repr(),
                m.repr(),
                y.repr(),
                k.repr()
            ),
            Some((c, m, y, k)) => self.set_color(true, StandardColor::Cmyk(c, m, y, k), "DeviceCMYK"),
        }
    }

    /// Set CMYK color for nonstroking operations
    pub fn do_k(&mut self, c: &Value, m: &Value, y: &Value, k: &Value) {
        match safe_cmyk(c, m, y, k) {
            None => warn!(
                "Cannot set CMYK non-stroke color because not all values in ({}, {}, {}, {}) can be parsed as \
                 floats",
                c.repr(),
                m.repr(),
                y.repr(),
                k.repr()
            ),
            Some((c, m, y, k)) => self.set_color(false, StandardColor::Cmyk(c, m, y, k), "DeviceCMYK"),
        }
    }

    /// Parse color components into StandardColor (gray, RGB, or CMYK).
    ///
    /// Returns the parsed color, or None if invalid (with a warning naming `context`).
    fn _parse_color_components(&self, components: &[Value], context: &str) -> Option<StandardColor> {
        match components.len() {
            1 => {
                let gray = safe_float(&components[0]);
                if gray.is_none() {
                    warn!("Cannot set {context} color: {} is an invalid float value", components[0].repr());
                }
                gray.map(StandardColor::Gray)
            }
            3 => {
                let rgb = safe_rgb(&components[0], &components[1], &components[2]);
                if rgb.is_none() {
                    warn!(
                        "Cannot set {context} color: components {} cannot be parsed as RGB",
                        Value::list(components.to_vec()).repr()
                    );
                }
                rgb.map(|(r, g, b)| StandardColor::Rgb(r, g, b))
            }
            4 => {
                let cmyk = safe_cmyk(&components[0], &components[1], &components[2], &components[3]);
                if cmyk.is_none() {
                    warn!(
                        "Cannot set {context} color: components {} cannot be parsed as CMYK",
                        Value::list(components.to_vec()).repr()
                    );
                }
                cmyk.map(|(c, m, y, k)| StandardColor::Cmyk(c, m, y, k))
            }
            n => {
                warn!(
                    "Cannot set {context} color: {n} components specified, but only 1 (grayscale), 3 (RGB), and 4 \
                     (CMYK) are supported"
                );
                None
            }
        }
    }

    /// `do_SCN` (`stroke`) and `do_scn` (non-stroke): set the color, handling Pattern color
    /// spaces per ISO 32000-1:2008 4.5.5 (PDF 1.7) and ISO 32000-2:2020 8.7.3 (PDF 2.0):
    /// - Colored patterns (PaintType=1): single operand (pattern name)
    /// - Uncolored patterns (PaintType=2): n+1 operands (colors + pattern name)
    fn set_color_n(&mut self, stroke: bool) -> PyResult<()> {
        let cs = {
            let gs = self.graphicstate.borrow();
            if stroke { gs.scs.clone() } else { gs.ncs.clone() }
        };
        let (context, op) = if stroke { ("stroke", "SCN") } else { ("non-stroke", "scn") };
        let n = cs.ncomponents();

        let components = self.pop_value(n)?;
        let set = |this: &mut Self, color: Color| {
            let mut gs = this.graphicstate.borrow_mut();
            if stroke {
                gs.scolor = color;
            } else {
                gs.ncolor = color;
            }
        };
        if !py_eq(&Value::Int(components.len() as i64), n) {
            warn!(
                "Cannot set {context} color because expected {} components but got {}",
                n.py_str(),
                Value::list(components).repr()
            );
        } else if cs.name() != "Pattern" {
            // Standard colors (gray, RGB, CMYK) - common case
            if let Some(color) = self._parse_color_components(&components, context) {
                set(self, Color::Standard(color));
            }
        } else if !components.is_empty() {
            // Pattern color space (ISO 32000 8.7.3.2-3)
            // Last component is always the pattern name
            let pattern_component = components.last().unwrap();

            // Per spec: pattern name must be a name object (PSLiteral)
            let Value::Literal(lit) = pattern_component else {
                warn!(
                    "Pattern color space requires name object (PSLiteral), got {}: {}. Per ISO 32000 8.7.3.2, \
                     colored patterns use syntax '/name {op}'. Per ISO 32000 8.7.3.3, uncolored patterns use \
                     syntax 'c1...cn /name {op}'.",
                    pattern_component.type_name(),
                    pattern_component.repr()
                );
                return Ok(());
            };

            let pattern_name = crate::psparser::literal_name_of(lit);

            if components.len() == 1 {
                // Colored tiling pattern (PaintType=1): just pattern name
                debug!("Set {context} pattern (colored): {pattern_name}");
                set(self, Color::Pattern(pattern_name));
            } else {
                // Uncolored tiling pattern (PaintType=2):
                // color components + pattern name
                let base_color_components = &components[..components.len() - 1];

                // Parse base color using shared logic
                let ctx = format!("{context} (uncolored pattern)");
                let Some(base_color) = self._parse_color_components(base_color_components, &ctx) else {
                    return Ok(());
                };

                // Store as tuple: (base_color, pattern_name)
                debug!("Set {context} pattern (uncolored): {} + {pattern_name}", base_color.repr());
                set(self, Color::UncoloredPattern(base_color, pattern_name));
            }
        }
        Ok(())
    }

    /// Set color for stroking operations.
    pub fn do_SCN(&mut self) -> PyResult<()> {
        self.set_color_n(true)
    }

    /// Set color for nonstroking operations.
    pub fn do_scn(&mut self) -> PyResult<()> {
        self.set_color_n(false)
    }

    /// Set color for stroking operations
    pub fn do_SC(&mut self) -> PyResult<()> {
        self.do_SCN()
    }

    /// Set color for nonstroking operations
    pub fn do_sc(&mut self) -> PyResult<()> {
        self.do_scn()
    }

    /// Paint area defined by shading pattern
    pub fn do_sh(&mut self, _name: &Value) {}

    /// Begin text object
    ///
    /// Initializing the text matrix, Tm, and the text line matrix, Tlm, to
    /// the identity matrix. Text objects cannot be nested; a second BT cannot
    /// appear before an ET.
    pub fn do_BT(&mut self) {
        self.textstate.reset();
    }

    /// End a text object
    pub fn do_ET(&mut self) {}

    /// Begin compatibility section
    pub fn do_BX(&mut self) {}

    /// End compatibility section
    pub fn do_EX(&mut self) {}

    /// Define marked-content point
    pub fn do_MP(&mut self, tag: &Value) -> PyResult<()> {
        if let Value::Literal(tag) = tag {
            self.device.borrow_mut().do_tag(tag, None)?;
        } else {
            warn!("Cannot define marked-content point because {} is not a PSLiteral", tag.repr());
        }
        Ok(())
    }

    /// Define marked-content point with property list
    pub fn do_DP(&mut self, tag: &Value, props: &Value) -> PyResult<()> {
        if let Value::Literal(tag) = tag {
            self.device.borrow_mut().do_tag(tag, Some(props))?;
        } else {
            warn!("Cannot define marked-content point with property list because {} is not a PSLiteral", tag.repr());
        }
        Ok(())
    }

    /// Begin marked-content sequence
    pub fn do_BMC(&mut self, tag: &Value) -> PyResult<()> {
        if let Value::Literal(tag) = tag {
            self.device.borrow_mut().begin_tag(tag, None)?;
        } else {
            warn!("Cannot begin marked-content sequence because {} is not a PSLiteral", tag.repr());
        }
        Ok(())
    }

    /// Begin marked-content sequence with property list
    pub fn do_BDC(&mut self, tag: &Value, props: &Value) -> PyResult<()> {
        if let Value::Literal(tag) = tag {
            self.device.borrow_mut().begin_tag(tag, Some(props))?;
        } else {
            warn!("Cannot begin marked-content sequence with property list because {} is not a PSLiteral", tag.repr());
        }
        Ok(())
    }

    /// End marked-content sequence
    pub fn do_EMC(&mut self) -> PyResult<()> {
        self.device.borrow_mut().end_tag()
    }

    /// Set character spacing.
    ///
    /// Character spacing is used by the Tj, TJ, and ' operators.
    pub fn do_Tc(&mut self, space: &Value) {
        match safe_float(space) {
            None => warn!("Could not set character spacing because {} is an invalid float value", space.repr()),
            Some(charspace) => self.textstate.charspace = charspace,
        }
    }

    /// Set the word spacing.
    ///
    /// Word spacing is used by the Tj, TJ, and ' operators.
    pub fn do_Tw(&mut self, space: &Value) {
        match safe_float(space) {
            None => warn!("Could not set word spacing because {} is an invalid float value", space.repr()),
            Some(wordspace) => self.textstate.wordspace = wordspace,
        }
    }

    /// Set the horizontal scaling.
    pub fn do_Tz(&mut self, scale: &Value) {
        match safe_float(scale) {
            None => warn!("Could not set horizontal scaling because {} is an invalid float value", scale.repr()),
            Some(scale_f) => self.textstate.scaling = scale_f,
        }
    }

    /// Set the text leading.
    ///
    /// Text leading is used only by the T*, ', and " operators.
    pub fn do_TL(&mut self, leading: &Value) {
        match safe_float(leading) {
            None => warn!("Could not set text leading because {} is an invalid float value", leading.repr()),
            Some(leading_f) => self.textstate.leading = -leading_f,
        }
    }

    /// Set the text font
    ///
    /// :param fontid: the name of a font resource in the Font subdictionary
    ///     of the current resource dictionary
    /// :param fontsize: size is a number representing a scale factor.
    pub fn do_Tf(&mut self, fontid: &Value, fontsize: &Value) -> PyResult<()> {
        let name = literal_name(fontid)?;
        match self.fontmap.get(name.as_str()) {
            Some(font) => self.textstate.font = Some(font.clone()),
            None => {
                if settings::STRICT {
                    return Err(PyErr::new(
                        ExcKind::PDFInterpreterError,
                        format!("Undefined Font id: {}", fontid.repr()),
                    ));
                }
                self.textstate.font = Some(self.rsrcmgr.get_font(None, &Value::new_dict())?);
            }
        }

        match safe_float(fontsize) {
            None => warn!("Could not set text font because {} is an invalid float value", fontsize.repr()),
            Some(fontsize_f) => self.textstate.fontsize = fontsize_f,
        }
        Ok(())
    }

    /// Set the text rendering mode
    pub fn do_Tr(&mut self, render: &Value) -> PyResult<()> {
        match safe_int_obj(render)? {
            None => warn!("Could not set text rendering mode because {} is an invalid int value", render.repr()),
            Some(render_i) => self.textstate.render = render_i,
        }
        Ok(())
    }

    /// Set the text rise
    pub fn do_Ts(&mut self, rise: &Value) {
        match safe_float(rise) {
            None => warn!("Could not set text rise because {} is an invalid float value", rise.repr()),
            Some(rise_f) => self.textstate.rise = rise_f,
        }
    }

    /// Move to the start of the next line
    ///
    /// Offset from the start of the current line by (tx , ty).
    pub fn do_Td(&mut self, tx: &Value, ty: &Value) -> PyResult<()> {
        let tx_ = safe_float(tx);
        let ty_ = safe_float(ty);
        if let (Some(tx_), Some(ty_)) = (tx_, ty_) {
            let (a, b, c, d, e, f) = self.textstate.matrix;
            let e_new = tx_ * a + ty_ * c + e;
            let f_new = tx_ * b + ty_ * d + f;
            self.textstate.matrix = (a, b, c, d, e_new, f_new);
        } else if settings::STRICT {
            return Err(PyErr::new(
                ExcKind::PDFValueError,
                format!("Invalid offset ({}, {}) for Td", tx.repr(), ty.repr()),
            ));
        }

        self.textstate.linematrix = (0.0, 0.0);
        Ok(())
    }

    /// Move to the start of the next line.
    ///
    /// offset from the start of the current line by (tx , ty). As a side effect, this
    /// operator sets the leading parameter in the text state.
    pub fn do_TD(&mut self, tx: &Value, ty: &Value) -> PyResult<()> {
        let tx_ = safe_float(tx);
        let ty_ = safe_float(ty);

        if let (Some(tx_), Some(ty_)) = (tx_, ty_) {
            let (a, b, c, d, e, f) = self.textstate.matrix;
            let e_new = tx_ * a + ty_ * c + e;
            let f_new = tx_ * b + ty_ * d + f;
            self.textstate.matrix = (a, b, c, d, e_new, f_new);
        } else if settings::STRICT {
            // (Python's message is not an f-string.)
            return Err(PyErr::new(ExcKind::PDFValueError, "Invalid offset ({tx}, {ty}) for TD"));
        }

        if let Some(ty_) = ty_ {
            self.textstate.leading = ty_;
        }

        self.textstate.linematrix = (0.0, 0.0);
        Ok(())
    }

    /// Set text matrix and text line matrix
    pub fn do_Tm(&mut self, a: &Value, b: &Value, c: &Value, d: &Value, e: &Value, f: &Value) {
        match safe_matrix(a, b, c, d, e, f) {
            None => warn!(
                "Could not set text matrix because not all values in {} can be parsed as floats",
                Value::tuple(vec![a.clone(), b.clone(), c.clone(), d.clone(), e.clone(), f.clone()]).repr()
            ),
            Some(matrix) => {
                self.textstate.matrix = matrix;
                self.textstate.linematrix = (0.0, 0.0);
            }
        }
    }

    /// Move to start of next text line
    pub fn do_T_a(&mut self) {
        let (a, b, c, d, e, f) = self.textstate.matrix;
        self.textstate.matrix = (a, b, c, d, self.textstate.leading * c + e, self.textstate.leading * d + f);
        self.textstate.linematrix = (0.0, 0.0);
    }

    /// Show text, allowing individual glyph positioning
    pub fn do_TJ(&mut self, seq: &Value) -> PyResult<()> {
        if self.textstate.font.is_none() {
            if settings::STRICT {
                return Err(PyErr::new(ExcKind::PDFInterpreterError, "No font specified!"));
            }
            return Ok(());
        }
        let (ncs, gs) = {
            let g = self.graphicstate.borrow();
            (g.ncs.clone(), g.copy())
        };
        self.device.borrow_mut().render_string_raw(&mut self.textstate, seq, &ncs, &gs)
    }

    /// Show text
    pub fn do_Tj(&mut self, s: &Value) -> PyResult<()> {
        self.do_TJ(&Value::list(vec![s.clone()]))
    }

    /// Move to next line and show text
    ///
    /// The ' (single quote) operator.
    pub fn do__q(&mut self, s: &Value) -> PyResult<()> {
        self.do_T_a();
        self.do_TJ(&Value::list(vec![s.clone()]))
    }

    /// Set word and character spacing, move to next line, and show text
    ///
    /// The " (double quote) operator.
    pub fn do__w(&mut self, aw: &Value, ac: &Value, s: &Value) -> PyResult<()> {
        self.do_Tw(aw);
        self.do_Tc(ac);
        self.do_TJ(&Value::list(vec![s.clone()]))
    }

    /// Begin inline image object
    pub fn do_BI(&mut self) {}

    /// Begin inline image data
    pub fn do_ID(&mut self) {}

    /// End inline image object
    pub fn do_EI(&mut self, obj: &Value) -> PyResult<()> {
        if let Value::Stream(s) = obj
            && s.contains("W")
            && s.contains("H")
        {
            // DIVERGE(inline-image-id): Python uses `str(id(obj))`.
            let n = self.inline_counter.get() + 1;
            self.inline_counter.set(n);
            let iobjid = format!("inline{n}");
            let mut dev = self.device.borrow_mut();
            dev.begin_figure(&iobjid, &unit_rect(), &identity_values())?;
            dev.render_image(&iobjid, s)?;
            dev.end_figure(&iobjid)?;
        }
        Ok(())
    }

    /// Invoke named XObject
    pub fn do_Do(&mut self, xobjid_arg: &Value) -> PyResult<()> {
        let xobjid = literal_name(xobjid_arg)?;
        let xobj = match self.xobjmap.get(xobjid.as_str()) {
            None => None,
            Some(v) => match stream_value(v) {
                Ok(s) => Some(s),
                Err(e) if e.is(ExcKind::KeyError) => None,
                Err(e) => return Err(e),
            },
        };
        let Some(xobj) = xobj else {
            if settings::STRICT {
                return Err(PyErr::new(
                    ExcKind::PDFInterpreterError,
                    format!("Undefined xobject id: {}", str_repr(&xobjid)),
                ));
            }
            return Ok(());
        };
        debug!("Processing xobj: {}", xobj.repr());
        let subtype = xobj.get("Subtype");
        let is_lit = |name: &str| matches!(&subtype, Some(Value::Literal(l)) if l.is(name));
        if is_lit(LITERAL_FORM) && xobj.contains("BBox") {
            let mut interpreter = self.subinterp();
            let bbox = list_value(&xobj.getitem("BBox")?)?;
            let matrix = match xobj.get("Matrix") {
                Some(m) => list_value(&m)?,
                None => identity_values(),
            };
            // According to PDF reference 1.7 section 4.9.1, XObjects in
            // earlier PDFs (prior to v1.2) use the page's Resources entry
            // instead of having their own Resources entry.
            let xobjres = xobj.get("Resources");
            let resources = match xobjres {
                Some(r) if r.truthy() => Value::Dict(dict_value(&r)?),
                _ => match &self.resources {
                    Value::Dict(d) => Value::dict(d.borrow().clone()),
                    Value::List(l) => Value::list(l.borrow().clone()),
                    other => {
                        return Err(PyErr::new(
                            ExcKind::AttributeError,
                            format!("'{}' object has no attribute 'copy'", other.type_name()),
                        ));
                    }
                },
            };
            self.device.borrow_mut().begin_figure(&xobjid, &bbox, &matrix)?;
            let (ctm, ctm_ints) = mult_matrix_value(&matrix, self.ctm, self.ctm_ints)?;
            if interpreter.depth > FORM_RECURSION_LIMIT {
                // DIVERGE(recursion-depth)
                return Err(PyErr::new(ExcKind::RecursionError, "maximum recursion depth exceeded"));
            }
            interpreter.render_contents_py(resources, vec![Value::Stream(xobj.clone())], ctm, ctm_ints)?;
            self.device.borrow_mut().end_figure(&xobjid)?;
        } else if is_lit(LITERAL_IMAGE) && xobj.contains("Width") && xobj.contains("Height") {
            let mut dev = self.device.borrow_mut();
            dev.begin_figure(&xobjid, &unit_rect(), &identity_values())?;
            dev.render_image(&xobjid, &xobj)?;
            dev.end_figure(&xobjid)?;
        } else {
            // unsupported xobject type.
        }
        Ok(())
    }

    /// `interpreter.process_page(page)`.
    pub fn process_page(&mut self, page: &PDFPage) -> PyResult<()> {
        debug!("Processing page: {}", page.repr());
        let (x0, y0, x1, y1) = page.mediabox;
        let ctm = if page.rotate == 90 {
            (0.0, -1.0, 1.0, 0.0, -y0, x1)
        } else if page.rotate == 180 {
            (-1.0, 0.0, 0.0, -1.0, x1, y1)
        } else if page.rotate == 270 {
            (0.0, 1.0, -1.0, 0.0, y1, -x0)
        } else {
            (1.0, 0.0, 0.0, 1.0, -x0, -y0)
        };
        // The first four entries are Python ints.
        let ctm_ints = [true, true, true, true, false, false];
        self.device.borrow_mut().begin_page(page, ctm)?;
        self.render_contents_py(page.resources.clone(), page.contents.clone(), ctm, ctm_ints)?;
        self.device.borrow_mut().end_page(page)?;
        Ok(())
    }

    /// Render the content streams.
    ///
    /// This method may be called recursively.
    pub fn render_contents(&mut self, resources: Value, streams: Vec<Value>, ctm: Matrix) -> PyResult<()> {
        self.render_contents_py(resources, streams, ctm, FLOAT_MATRIX)
    }

    /// `render_contents(resources, streams, ctm)` where the entries `ctm_ints` of `ctm` are
    /// Python ints.
    pub fn render_contents_py(
        &mut self,
        resources: Value,
        streams: Vec<Value>,
        ctm: Matrix,
        ctm_ints: MatrixInts,
    ) -> PyResult<()> {
        debug!("render_contents: resources={}, ctm={}", resources.repr(), matrix_repr(&ctm));
        self.init_resources(resources)?;
        self.init_state_py(ctm, ctm_ints);
        self.execute(&streams)
    }

    pub fn execute(&mut self, streams: &[Value]) -> PyResult<()> {
        // Detect and prevent circular references in content streams
        // (including Form XObjects).
        // We track stream IDs being executed in the current interpreter and
        // all parent interpreters. If a stream is already being processed
        // in the call stack, we skip
        // it to prevent infinite recursion (CWE-835 vulnerability).
        let mut valid_streams: Vec<Value> = Vec::new();
        self.stream_ids.clear();
        for obj in streams {
            let stream = stream_value(obj)?;
            let Some(objid) = stream.objid.get() else {
                // Inline streams without object IDs can't be tracked for circular refs
                warn!("Execute called on non-indirect object (inline image?) {}", stream.repr());
                continue;
            };
            if self.parent_stream_ids.contains(&objid) {
                warn!("Refusing to execute circular reference to content stream {objid}");
            } else {
                valid_streams.push(Value::Stream(stream));
                self.stream_ids.insert(objid);
            }
        }
        let mut parser = match PDFContentParser::new(valid_streams) {
            Ok(p) => Box::new(p),
            // empty page
            Err(e) if e.is(ExcKind::PSEOF) => return Ok(()),
            Err(e) => return Err(e),
        };
        loop {
            let obj = match parser.nextobject() {
                Ok((_, obj)) => obj,
                Err(e) if e.is(ExcKind::PSEOF) => break,
                Err(e) => return Err(e),
            };
            if let Value::Keyword(kw) = &obj {
                let name = keyword_name_of(kw);
                let method: std::borrow::Cow<str> = if name.contains(['*', '"', '\'']) {
                    name.replace('*', "_a").replace('"', "_w").replace('\'', "_q").into()
                } else {
                    (&*name).into()
                };
                match op_nargs(&method) {
                    Some(nargs) => {
                        if nargs > 0 {
                            let args = self.pop(nargs);
                            debug!("exec: {name} {args:?}");
                            if args.len() == nargs {
                                self.call(&method, &args)?;
                            }
                        } else {
                            debug!("exec: {name}");
                            self.call(&method, &[])?;
                        }
                    }
                    None => {
                        if settings::STRICT {
                            let error_msg = format!("Unknown operator: {}", str_repr(&name));
                            return Err(PyErr::new(ExcKind::PDFInterpreterError, error_msg));
                        }
                    }
                }
            } else {
                self.push(obj);
            }
        }
        Ok(())
    }

    /// `getattr(self, "do_" + method)(*args)`.
    fn call(&mut self, method: &str, a: &[Value]) -> PyResult<()> {
        match method {
            "q" => self.do_q(),
            "Q" => self.do_Q(),
            "cm" => self.do_cm(&a[0], &a[1], &a[2], &a[3], &a[4], &a[5]),
            "w" => self.do_w(&a[0])?,
            "J" => self.do_J(&a[0]),
            "j" => self.do_j(&a[0]),
            "M" => self.do_M(&a[0]),
            "d" => self.do_d(&a[0], &a[1]),
            "ri" => self.do_ri(&a[0]),
            "i" => self.do_i(&a[0]),
            "gs" => self.do_gs(&a[0]),
            "m" => self.do_m(&a[0], &a[1]),
            "l" => self.do_l(&a[0], &a[1]),
            "c" => self.do_c(&a[0], &a[1], &a[2], &a[3], &a[4], &a[5]),
            "v" => self.do_v(&a[0], &a[1], &a[2], &a[3]),
            "y" => self.do_y(&a[0], &a[1], &a[2], &a[3]),
            "h" => self.do_h(),
            "re" => self.do_re(&a[0], &a[1], &a[2], &a[3]),
            "S" => self.do_S()?,
            "s" => self.do_s()?,
            "f" => self.do_f()?,
            "F" => self.do_F(),
            "f_a" => self.do_f_a()?,
            "B" => self.do_B()?,
            "B_a" => self.do_B_a()?,
            "b" => self.do_b()?,
            "b_a" => self.do_b_a()?,
            "n" => self.do_n(),
            "W" => self.do_W(),
            "W_a" => self.do_W_a(),
            "CS" => self.do_CS(&a[0])?,
            "cs" => self.do_cs(&a[0])?,
            "G" => self.do_G(&a[0]),
            "g" => self.do_g(&a[0]),
            "RG" => self.do_RG(&a[0], &a[1], &a[2]),
            "rg" => self.do_rg(&a[0], &a[1], &a[2]),
            "K" => self.do_K(&a[0], &a[1], &a[2], &a[3]),
            "k" => self.do_k(&a[0], &a[1], &a[2], &a[3]),
            "SCN" => self.do_SCN()?,
            "scn" => self.do_scn()?,
            "SC" => self.do_SC()?,
            "sc" => self.do_sc()?,
            "sh" => self.do_sh(&a[0]),
            "BT" => self.do_BT(),
            "ET" => self.do_ET(),
            "BX" => self.do_BX(),
            "EX" => self.do_EX(),
            "MP" => self.do_MP(&a[0])?,
            "DP" => self.do_DP(&a[0], &a[1])?,
            "BMC" => self.do_BMC(&a[0])?,
            "BDC" => self.do_BDC(&a[0], &a[1])?,
            "EMC" => self.do_EMC()?,
            "Tc" => self.do_Tc(&a[0]),
            "Tw" => self.do_Tw(&a[0]),
            "Tz" => self.do_Tz(&a[0]),
            "TL" => self.do_TL(&a[0]),
            "Tf" => self.do_Tf(&a[0], &a[1])?,
            "Tr" => self.do_Tr(&a[0])?,
            "Ts" => self.do_Ts(&a[0]),
            "Td" => self.do_Td(&a[0], &a[1])?,
            "TD" => self.do_TD(&a[0], &a[1])?,
            "Tm" => self.do_Tm(&a[0], &a[1], &a[2], &a[3], &a[4], &a[5]),
            "T_a" => self.do_T_a(),
            "TJ" => self.do_TJ(&a[0])?,
            "Tj" => self.do_Tj(&a[0])?,
            "_q" => self.do__q(&a[0])?,
            "_w" => self.do__w(&a[0], &a[1], &a[2])?,
            "BI" => self.do_BI(),
            "ID" => self.do_ID(),
            "EI" => self.do_EI(&a[0])?,
            "Do" => self.do_Do(&a[0])?,
            _ => unreachable!("op_nargs lists every method"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Ports of the interpreter tests of tests/test_converter.py (`test_pattern_operators`) and
    // tests/test_pdffont.py (`test_cmap_font_12`), plus checks of the Python rules the
    // interpreter reproduces (expected values from Python 3.14 and pdfminer.six 20260107).
    use super::*;
    use crate::pdfdevice::{PDFTextDevice, PlainPDFDevice, text_render_string};
    use crate::pdfdocument::PDFDocument;
    use crate::pdfparser::PDFParser;
    use crate::psparser::LIT;

    fn interp() -> PDFPageInterpreter {
        let device: Rc<RefCell<dyn PDFDevice>> = Rc::new(RefCell::new(PlainPDFDevice::default()));
        PDFPageInterpreter::new(PDFResourceManager::new(true), device)
    }

    #[test]
    fn test_pattern_operators() {
        let mut interpreter = interp();
        // Initialize the interpreter state (creates graphicstate)
        interpreter.init_resources(Value::Dict(Value::new_dict())).unwrap();
        interpreter.init_state((1.0, 0.0, 0.0, 1.0, 0.0, 0.0));
        let set_scs =
            |i: &mut PDFPageInterpreter, n: i64| i.graphicstate.borrow_mut().scs = PDFColorSpace::new("Pattern", n);
        let set_ncs =
            |i: &mut PDFPageInterpreter, n: i64| i.graphicstate.borrow_mut().ncs = PDFColorSpace::new("Pattern", n);
        let scolor = |i: &PDFPageInterpreter| i.graphicstate.borrow().scolor.repr();
        let ncolor = |i: &PDFPageInterpreter| i.graphicstate.borrow().ncolor.repr();

        // Test 1: Colored pattern (stroking)
        set_scs(&mut interpreter, 1);
        interpreter.push(Value::Literal(LIT("P1444")));
        interpreter.do_SCN().unwrap();
        assert_eq!(interpreter.graphicstate.borrow().scolor, Color::Pattern("P1444".into()));

        // Test 2: Colored pattern (non-stroking)
        set_ncs(&mut interpreter, 1);
        interpreter.push(Value::Literal(LIT("P1445")));
        interpreter.do_scn().unwrap();
        assert_eq!(interpreter.graphicstate.borrow().ncolor, Color::Pattern("P1445".into()));

        // Test 3: Uncolored pattern with gray
        set_scs(&mut interpreter, 2);
        interpreter.push(Value::Float(0.5));
        interpreter.push(Value::Literal(LIT("P2000")));
        interpreter.do_SCN().unwrap();
        assert_eq!(scolor(&interpreter), "(0.5, 'P2000')");

        // Test 4: Uncolored pattern with RGB
        set_ncs(&mut interpreter, 4);
        for v in [0.77, 0.2, 0.0] {
            interpreter.push(Value::Float(v));
        }
        interpreter.push(Value::Literal(LIT("P3000")));
        interpreter.do_scn().unwrap();
        assert_eq!(ncolor(&interpreter), "((0.77, 0.2, 0.0), 'P3000')");

        // Test 5: Uncolored pattern with CMYK
        set_scs(&mut interpreter, 5);
        for v in [0.1, 0.2, 0.3, 0.4] {
            interpreter.push(Value::Float(v));
        }
        interpreter.push(Value::Literal(LIT("P4000")));
        interpreter.do_SCN().unwrap();
        assert_eq!(scolor(&interpreter), "((0.1, 0.2, 0.3, 0.4), 'P4000')");

        // Test 6: Invalid pattern (non-PSLiteral): a warning, the color is kept
        set_ncs(&mut interpreter, 1);
        interpreter.push(Value::Float(0.5));
        interpreter.do_scn().unwrap();
        assert_eq!(ncolor(&interpreter), "((0.77, 0.2, 0.0), 'P3000')");

        // Test 7: Invalid uncolored pattern (non-PSLiteral for pattern name)
        set_scs(&mut interpreter, 2);
        interpreter.push(Value::Float(0.5));
        interpreter.push(Value::Int(999));
        interpreter.do_SCN().unwrap();
        assert_eq!(scolor(&interpreter), "((0.1, 0.2, 0.3, 0.4), 'P4000')");
        assert!(interpreter.argstack.is_empty());
    }

    /// `scn` pops `ncomponents` operands with Python's slicing, whatever `/N` holds.
    #[test]
    fn scn_with_odd_ncomponents() {
        let mut i = interp();
        i.init_resources(Value::None).unwrap();
        i.init_state(MATRIX_IDENTITY);
        let args = |i: &mut PDFPageInterpreter| i.argstack = (1..=5).map(Value::Int).collect();
        let set = |i: &mut PDFPageInterpreter, n: Value| {
            i.graphicstate.borrow_mut().ncs = PDFColorSpace::new_value("ICCBased", n)
        };
        // N = -2: x = stack[2:], stack = stack[:2]
        args(&mut i);
        set(&mut i, Value::Int(-2));
        i.do_scn().unwrap();
        assert_eq!(i.argstack.len(), 2);
        // N = True pops one, like 1
        args(&mut i);
        set(&mut i, Value::Bool(true));
        i.do_scn().unwrap();
        assert_eq!(i.argstack.len(), 4);
        assert_eq!(i.graphicstate.borrow().ncolor.repr(), "5.0");
        // N = 3.0: TypeError (slice indices must be integers)
        args(&mut i);
        set(&mut i, Value::Float(3.0));
        assert_eq!(i.do_scn().unwrap_err().kind, ExcKind::TypeError);
        // N = 0.0 == 0: nothing popped, no color
        set(&mut i, Value::Float(0.0));
        i.do_scn().unwrap();
        assert_eq!(i.argstack.len(), 5);
        // N = /Name: TypeError (bad operand type for unary -)
        set(&mut i, Value::Literal(LIT("x")));
        assert_eq!(i.do_scn().unwrap_err().kind, ExcKind::TypeError);
    }

    /// Every `do_*` method of Python's `PDFPageInterpreter` and its operand count
    /// (`[(n[3:], getattr(P, n).__code__.co_argcount - 1) for n in dir(P) if n.startswith("do_")]`).
    #[test]
    fn operator_table_matches_python() {
        let python: &[(&str, usize)] = &[
            ("B", 0),
            ("BDC", 2),
            ("BI", 0),
            ("BMC", 1),
            ("BT", 0),
            ("BX", 0),
            ("B_a", 0),
            ("CS", 1),
            ("DP", 2),
            ("Do", 1),
            ("EI", 1),
            ("EMC", 0),
            ("ET", 0),
            ("EX", 0),
            ("F", 0),
            ("G", 1),
            ("ID", 0),
            ("J", 1),
            ("K", 4),
            ("M", 1),
            ("MP", 1),
            ("Q", 0),
            ("RG", 3),
            ("S", 0),
            ("SC", 0),
            ("SCN", 0),
            ("TD", 2),
            ("TJ", 1),
            ("TL", 1),
            ("T_a", 0),
            ("Tc", 1),
            ("Td", 2),
            ("Tf", 2),
            ("Tj", 1),
            ("Tm", 6),
            ("Tr", 1),
            ("Ts", 1),
            ("Tw", 1),
            ("Tz", 1),
            ("W", 0),
            ("W_a", 0),
            ("_q", 1),
            ("_w", 3),
            ("b", 0),
            ("b_a", 0),
            ("c", 6),
            ("cm", 6),
            ("cs", 1),
            ("d", 2),
            ("f", 0),
            ("f_a", 0),
            ("g", 1),
            ("gs", 1),
            ("h", 0),
            ("i", 1),
            ("j", 1),
            ("k", 4),
            ("l", 2),
            ("m", 2),
            ("n", 0),
            ("q", 0),
            ("re", 4),
            ("rg", 3),
            ("ri", 1),
            ("s", 0),
            ("sc", 0),
            ("scn", 0),
            ("sh", 1),
            ("v", 4),
            ("w", 1),
            ("y", 4),
        ];
        for (name, n) in python {
            assert_eq!(op_nargs(name), Some(*n), "{name}");
        }
        for name in ["", "a", "T*", "_a", "do_q", "BI2", "Tq"] {
            assert_eq!(op_nargs(name), None, "{name}");
        }
    }

    /// `re.sub(rb"(\x0d\x0a|[\x0d\x0a])$", b"", data)`.
    #[test]
    fn inline_data_trailing_eol() {
        let cases: &[(&[u8], &[u8])] = &[
            (b"abc", b"abc"),
            (b"abc\r\n", b"abc"),
            (b"abc\n\n", b"abc"),
            (b"abc\r\n\n", b"abc"),
            (b"abc\n\r\n", b"abc\n"),
            (b"abc\r\r", b"abc\r"),
            (b"abc\n\r", b"abc\n"),
            (b"\n", b""),
            (b"\r\n", b""),
            (b"abc\r\n\r\n", b"abc\r\n"),
            (b"x\n\n\n", b"x\n"),
            (b"", b""),
            (b"\r", b""),
            (b"\n\r\n\n", b"\n"),
        ];
        for (input, want) in cases {
            let mut d = input.to_vec();
            strip_trailing_eol(&mut d);
            assert_eq!(&d[..], *want, "{input:?}");
        }
    }

    fn content_objects(streams: &[&[u8]]) -> Result<Vec<String>, &'static str> {
        let streams: Vec<Value> = streams
            .iter()
            .map(|s| Value::Stream(Rc::new(PDFStream::new(Value::new_dict(), Rc::from(*s), None))))
            .collect();
        let mut p = PDFContentParser::new(streams).map_err(|e| e.kind.name())?;
        let mut out = Vec::new();
        loop {
            match p.nextobject() {
                Ok((pos, Value::Stream(s))) => out.push(format!(
                    "{pos}:stream {} {}",
                    crate::pyrt::value::dict_repr(&s.attrs.borrow()),
                    crate::pyrt::fmt::bytes_repr(&s.get_rawdata().unwrap())
                )),
                Ok((pos, o)) => out.push(format!("{pos}:{}", o.repr())),
                Err(e) if e.is(ExcKind::PSEOF) => break,
                Err(e) => return Err(e.kind.name()),
            }
        }
        Ok(out)
    }

    /// `PDFContentParser` on inline images and split streams.
    #[test]
    fn content_parser_inline_images() {
        let ok = |v: &[&str]| Ok(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            content_objects(&[b"q BI /W 2 /H 1 /CS /G ID \x00\xffEI Q"]),
            ok(&["0:/b'q'", "25:stream {'W': 2, 'H': 1, 'CS': /'G'} b'\\x00\\xff'", "25:/b'EI'", "30:/b'Q'"])
        );
        // "EI" inside the data must be followed by whitespace to end it
        assert_eq!(
            content_objects(&[b"BI /W 1 /H 1 ID aEIbEI\nQ"]),
            ok(&["16:stream {'W': 1, 'H': 1} b'aEIb'", "16:/b'EI'", "23:/b'Q'"])
        );
        // ASCII85: the data ends at ~> (kept), and EI stays in the stream
        assert_eq!(
            content_objects(&[b"BI /W 1 /H 1 /F /A85 ID z~> EI"]),
            ok(&["24:stream {'W': 1, 'H': 1, 'F': /'A85'} b'z~>'", "28:/b'EI'"])
        );
        // an odd dictionary is ignored (PSTypeError), the data is parsed as content
        assert_eq!(content_objects(&[b"BI /W ID 1 2 EI"]), ok(&["9:1", "11:2", "13:/b'EI'"]));
        assert_eq!(content_objects(&[b"BI /F 5 ID x EI"]), Err("TypeError"));
        assert_eq!(content_objects(&[b"BI /F [] ID x EI"]), Err("IndexError"));
        // data across streams; a truncated image ends the content (PSEOF)
        assert_eq!(
            content_objects(&[b"BI /W 1 ID ab", b"cd EI Q"]),
            ok(&["11:stream {'W': 1} b'abcd '", "11:/b'EI'", "6:/b'Q'"])
        );
        assert_eq!(content_objects(&[b"BI /W 1 ID abc"]), ok(&[]));
        // no stream at all: PSEOF from the constructor
        assert_eq!(content_objects(&[]), Err("PSEOF"));
        // a token split across two streams (#1157)
        assert_eq!(
            content_objects(&[b"1 0 0 1 0 0 c", b"m"]),
            ok(&["0:1", "2:0", "4:0", "6:1", "8:0", "10:0", "12:/b'c'", "0:/b'm'"])
        );
    }

    /// The form's `/Matrix` times the CTM, with Python's int CTM entries.
    #[test]
    fn mult_matrix_value_matches_python() {
        let page = [true, true, true, true, false, false];
        let m = |v: &[Value]| mult_matrix_value(v, (1.0, 0.0, 0.0, 1.0, -0.0, -0.0), page);
        let ints = |xs: [i64; 6]| xs.iter().map(|&i| Value::Int(i)).collect::<Vec<_>>();
        assert_eq!(m(&ints([2, 0, 0, 3, 4, 5])).unwrap(), ((2.0, 0.0, 0.0, 3.0, 4.0, 5.0), page));
        let mut bad = ints([1, 0, 0, 1, 0, 0]);
        bad[2] = Value::bytes(b"x");
        assert_eq!(m(&bad).unwrap_err().msg, "can't concat int to bytes");
        let e = mult_matrix_value(&bad, MATRIX_IDENTITY, FLOAT_MATRIX).unwrap_err();
        assert_eq!(e.msg, "can't multiply sequence by non-int of type 'float'");
        assert_eq!(
            m(&ints([1, 0, 0, 1, 0, 0])[..5]).unwrap_err().msg,
            "not enough values to unpack (expected 6, got 5)"
        );
    }

    /// A PDF with a chain of `n` nested form XObjects (each invokes the next, the last fills a
    /// rectangle).
    fn form_chain(n: usize) -> Vec<u8> {
        let mut objs: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /F 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
            b"<< /Length 5 >>\nstream\n/F Do\nendstream".to_vec(),
        ];
        for i in 0..n {
            let (c, res) = if i + 1 < n {
                ("/F Do".to_string(), format!("<< /XObject << /F {} 0 R >> >>", 5 + i + 1))
            } else {
                ("0 0 1 1 re f".to_string(), "<< >>".to_string())
            };
            objs.push(
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 1 1] /Resources {res} /Length {} >>\nstream\n{c}\nendstream",
                    c.len()
                )
                .into_bytes(),
            );
        }
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offs = vec![];
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out.extend(format!("{} 0 obj\n", i + 1).bytes());
            out.extend(o);
            out.extend(b"\nendobj\n");
        }
        let x = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
        for o in offs {
            out.extend(format!("{o:010} 00000 n \n").bytes());
        }
        out.extend(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).bytes());
        out
    }

    #[derive(Default)]
    struct Figures {
        ctm: Option<Matrix>,
        figs: usize,
        paths: usize,
    }

    impl PDFDevice for Figures {
        fn ctm(&self) -> Option<Matrix> {
            self.ctm
        }
        fn set_ctm(&mut self, ctm: Matrix) {
            self.ctm = Some(ctm);
        }
        fn begin_figure(&mut self, _: &str, _: &[Value], _: &[Value]) -> PyResult<()> {
            self.figs += 1;
            Ok(())
        }
        fn paint_path(&mut self, _: &PDFGraphicStateRef, _: bool, _: bool, _: bool, _: &[PathSegment]) -> PyResult<()> {
            self.paths += 1;
            Ok(())
        }
    }

    fn run_chain(n: usize) -> (PyResult<()>, usize, usize) {
        let doc = PDFDocument::new(PDFParser::new(BytesIO::new(Rc::from(form_chain(n)))), "", true, true).unwrap();
        let page = PDFPage::create_pages(&doc).next().unwrap().unwrap();
        let dev = Rc::new(RefCell::new(Figures::default()));
        let mut i = PDFPageInterpreter::new(PDFResourceManager::new(true), dev.clone());
        let r = i.process_page(&page);
        let d = dev.borrow();
        (r, d.figs, d.paths)
    }

    /// pdfminer.six (under `extract_pages`) handles 328 nested forms and raises `RecursionError`
    /// at 329. (A thread with a large stack: debug builds use ~13 KB per level.)
    #[test]
    fn nested_forms_recursion_limit() {
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(|| {
                let (r, figs, paths) = run_chain(328);
                assert!(r.is_ok());
                assert_eq!((figs, paths), (328, 1));
                let (r, figs, _) = run_chain(329);
                assert_eq!(r.unwrap_err().kind, ExcKind::RecursionError);
                assert_eq!(figs, 329);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    struct Chars {
        ctm: Option<Matrix>,
        n: usize,
    }

    impl PDFDevice for Chars {
        fn ctm(&self) -> Option<Matrix> {
            self.ctm
        }
        fn set_ctm(&mut self, ctm: Matrix) {
            self.ctm = Some(ctm);
        }
        fn render_string(
            &mut self,
            textstate: &mut PDFTextState,
            seq: &[Value],
            ncs: &PDFColorSpace,
            graphicstate: &PDFGraphicStateRef,
        ) -> PyResult<()> {
            text_render_string(self, textstate, seq, ncs, graphicstate)
        }
    }

    impl PDFTextDevice for Chars {
        fn render_char(
            &mut self,
            _: Matrix,
            font: &Rc<PDFFont>,
            fontsize: f64,
            scaling: f64,
            _: f64,
            cid: i64,
            _: &PDFColorSpace,
            _: &PDFGraphicStateRef,
        ) -> PyResult<f64> {
            self.n += 1;
            Ok(font.char_width(cid)? * fontsize * scaling)
        }
    }

    /// tests/test_pdffont.py::test_cmap_font_12 (with a text device instead of the aggregator).
    #[test]
    fn test_cmap_font_12() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../upstream/pdfminer.six/samples/contrib/issue-598-cmap-other-fonts.pdf");
        let data = std::fs::read(path).unwrap();
        let doc = PDFDocument::new(PDFParser::new(BytesIO::new(Rc::from(data))), "", true, true).unwrap();
        let dev = Rc::new(RefCell::new(Chars { ctm: None, n: 0 }));
        let mut i = PDFPageInterpreter::new(PDFResourceManager::new(true), dev.clone());
        for page in PDFPage::create_pages(&doc) {
            i.process_page(&page.unwrap()).unwrap();
        }
        assert!(dev.borrow().n > 0);
    }

    /// `5 TJ` raises TypeError in a text device (iterating an int), `(ab) TJ` iterates bytes as
    /// numbers; the base device ignores both.
    #[test]
    fn tj_operand_iteration() {
        let mut i = interp();
        i.init_resources(Value::None).unwrap();
        i.init_state(MATRIX_IDENTITY);
        i.textstate.font = Some(i.rsrcmgr.get_font(None, &Value::new_dict()).unwrap());
        i.do_TJ(&Value::Int(5)).unwrap();
        let dev = Rc::new(RefCell::new(Chars { ctm: None, n: 0 }));
        i.device = dev.clone();
        i.init_state(MATRIX_IDENTITY);
        i.textstate.font = Some(i.rsrcmgr.get_font(None, &Value::new_dict()).unwrap());
        i.textstate.fontsize = 10.0;
        assert_eq!(i.do_TJ(&Value::Int(5)).unwrap_err().kind, ExcKind::TypeError);
        i.do_TJ(&Value::bytes(b"ab")).unwrap();
        assert_eq!(dev.borrow().n, 0);
        assert_eq!(i.textstate.linematrix, (-(97.0 * 0.01) - 98.0 * 0.01, 0.0));
    }
}
