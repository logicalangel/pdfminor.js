//! Port of `pdfminer/pdfdevice.py`.
//!
//! `PDFDevice` and its subclasses (`PDFTextDevice`, `TagExtractor`, the converters) become the
//! [`PDFDevice`] trait: one method per overridable Python method, with the base class's no-op as
//! the default. The interpreter holds its device as `Rc<RefCell<dyn PDFDevice>>`.
//!
//! `PDFTextDevice` is the [`PDFTextDevice`] trait (its `render_char` hook) plus the free
//! functions [`text_render_string`], [`render_string_horizontal`] and [`render_string_vertical`]:
//! a text device (e.g. `PDFLayoutAnalyzer`) implements `render_char` and forwards
//! `PDFDevice::render_string` to `text_render_string(self, ...)`.

use std::rc::Rc;

use log::warn;

use crate::pdfcolor::PDFColorSpace;
use crate::pdffont::PDFFont;
use crate::pdfinterp::{MatrixInts, PDFGraphicStateRef, PDFTextState};
use crate::pdfpage::PDFPage;
use crate::pdftypes::PDFStream;
use crate::psparser::{NameType, PSLiteral};
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::pystr::PyStr;
use crate::pyrt::textio::{AnyIO, encode_str};
use crate::pyrt::value::Value;
use crate::utils::{self, Matrix, PathSegment, Point};

/// `for obj in seq`: the items Python's iteration yields for the operand of `TJ` (`PDFTextSeq`
/// is whatever the content stream put there). A `bytes` operand yields ints, a `str` its
/// characters, a dict its keys; a non-iterable raises `TypeError` (a `PDFStream` raises from
/// `stream[0]`, as it has `__getitem__` but no `__iter__`).
pub fn iter_text_seq(seq: &Value) -> PyResult<Vec<Value>> {
    match seq {
        Value::List(l) => Ok(l.borrow().clone()),
        Value::Tuple(t) => Ok(t.to_vec()),
        Value::Bytes(b) => Ok(b.iter().map(|&c| Value::Int(c as i64)).collect()),
        Value::Str(s) => Ok(s.chars().map(|c| Value::str(&c.to_string())).collect()),
        Value::Dict(d) => Ok(d.borrow().keys().map(|k| Value::Str(k.clone())).collect()),
        Value::Stream(s) => Err(s.getitem("0").unwrap_err()),
        other => Err(PyErr::new(ExcKind::TypeError, format!("'{}' object is not iterable", other.type_name()))),
    }
}

/// `PDFDevice`: translates the output of `PDFPageInterpreter` to the output that is needed.
pub trait PDFDevice {
    /// `self.ctm`.
    fn ctm(&self) -> Option<Matrix>;

    /// `set_ctm(ctm)`.
    fn set_ctm(&mut self, ctm: Matrix);

    /// What the interpreter calls: `set_ctm(ctm)`, telling which entries of `ctm` are Python
    /// ints (see [`MatrixInts`]); only a device that multiplies the CTM with untyped operands
    /// (`PDFLayoutAnalyzer.begin_figure`) needs them.
    fn set_ctm_py(&mut self, ctm: Matrix, _ints: MatrixInts) {
        self.set_ctm(ctm);
    }

    fn close(&mut self) -> PyResult<()> {
        Ok(())
    }

    /// `begin_tag(tag, props=None)`: `props` is the untyped operand (`PDFStackT`).
    fn begin_tag(&mut self, _tag: &PSLiteral, _props: Option<&Value>) -> PyResult<()> {
        Ok(())
    }

    fn end_tag(&mut self) -> PyResult<()> {
        Ok(())
    }

    fn do_tag(&mut self, _tag: &PSLiteral, _props: Option<&Value>) -> PyResult<()> {
        Ok(())
    }

    fn begin_page(&mut self, _page: &PDFPage, _ctm: Matrix) -> PyResult<()> {
        Ok(())
    }

    fn end_page(&mut self, _page: &PDFPage) -> PyResult<()> {
        Ok(())
    }

    /// `begin_figure(name, bbox, matrix)`. `bbox` and `matrix` are what the interpreter passes:
    /// `list_value(xobj["BBox"])` / `list_value(xobj.get("Matrix", MATRIX_IDENTITY))` for form
    /// XObjects (untyped items, any length), `(0, 0, 1, 1)` / `MATRIX_IDENTITY` (ints) for images.
    fn begin_figure(&mut self, _name: &str, _bbox: &[Value], _matrix: &[Value]) -> PyResult<()> {
        Ok(())
    }

    fn end_figure(&mut self, _name: &str) -> PyResult<()> {
        Ok(())
    }

    fn paint_path(
        &mut self,
        _graphicstate: &PDFGraphicStateRef,
        _stroke: bool,
        _fill: bool,
        _evenodd: bool,
        _path: &[PathSegment],
    ) -> PyResult<()> {
        Ok(())
    }

    fn render_image(&mut self, _name: &str, _stream: &Rc<PDFStream>) -> PyResult<()> {
        Ok(())
    }

    /// `render_string(textstate, seq, ncs, graphicstate)`: `seq` is the `TJ` array, already
    /// iterated (see [`PDFDevice::render_string_raw`]); text devices update
    /// `textstate.linematrix`.
    fn render_string(
        &mut self,
        _textstate: &mut PDFTextState,
        _seq: &[Value],
        _ncs: &PDFColorSpace,
        _graphicstate: &PDFGraphicStateRef,
    ) -> PyResult<()> {
        Ok(())
    }

    /// What the interpreter calls: `render_string` with the raw `TJ` operand. Every device that
    /// handles text iterates `seq` first (`for obj in seq`), so the default iterates it like
    /// Python ([`iter_text_seq`]: `5 TJ` raises `TypeError`) and calls
    /// [`PDFDevice::render_string`] with the items. Only a device that ignores the text (the base
    /// `PDFDevice`) overrides it to do nothing.
    fn render_string_raw(
        &mut self,
        textstate: &mut PDFTextState,
        seq: &Value,
        ncs: &PDFColorSpace,
        graphicstate: &PDFGraphicStateRef,
    ) -> PyResult<()> {
        let items = iter_text_seq(seq)?;
        self.render_string(textstate, &items, ncs, graphicstate)
    }
}

/// The base `PDFDevice` class itself: does nothing (and, unlike text devices, never iterates
/// the text operands).
#[derive(Default)]
pub struct PlainPDFDevice {
    pub ctm: Option<Matrix>,
}

impl PDFDevice for PlainPDFDevice {
    fn ctm(&self) -> Option<Matrix> {
        self.ctm
    }
    fn set_ctm(&mut self, ctm: Matrix) {
        self.ctm = Some(ctm);
    }
    fn render_string_raw(
        &mut self,
        _textstate: &mut PDFTextState,
        _seq: &Value,
        _ncs: &PDFColorSpace,
        _graphicstate: &PDFGraphicStateRef,
    ) -> PyResult<()> {
        Ok(())
    }
}

/// `PDFTextDevice`: the `render_char` hook of the text devices; the positioning logic is
/// [`text_render_string`].
pub trait PDFTextDevice: PDFDevice {
    /// `render_char(...)`: draws one character and returns its advance (`0` in the base class).
    fn render_char(
        &mut self,
        _matrix: Matrix,
        _font: &Rc<PDFFont>,
        _fontsize: f64,
        _scaling: f64,
        _rise: f64,
        _cid: i64,
        _ncs: &PDFColorSpace,
        _graphicstate: &PDFGraphicStateRef,
    ) -> PyResult<f64> {
        Ok(0.0)
    }
}

/// `PDFTextDevice.render_string(textstate, seq, ncs, graphicstate)`.
pub fn text_render_string<D: PDFTextDevice + ?Sized>(
    dev: &mut D,
    textstate: &mut PDFTextState,
    seq: &[Value],
    ncs: &PDFColorSpace,
    graphicstate: &PDFGraphicStateRef,
) -> PyResult<()> {
    let Some(ctm) = dev.ctm() else {
        return Err(PyErr::new(ExcKind::AssertionError, ""));
    };
    let matrix = utils::mult_matrix(textstate.matrix, ctm);
    let font = textstate.font.clone();
    let fontsize = textstate.fontsize;
    let scaling = textstate.scaling * 0.01;
    let charspace = textstate.charspace * scaling;
    let mut wordspace = textstate.wordspace * scaling;
    let rise = textstate.rise;
    let Some(font) = font else {
        return Err(PyErr::new(ExcKind::AssertionError, ""));
    };
    if font.is_multibyte() {
        wordspace = 0.0;
    }
    let dxscale = 0.001 * fontsize * scaling;
    let render = if font.is_vertical() { render_string_vertical } else { render_string_horizontal };
    textstate.linematrix = render(
        dev,
        seq,
        matrix,
        textstate.linematrix,
        &font,
        fontsize,
        scaling,
        charspace,
        wordspace,
        rise,
        dxscale,
        ncs,
        graphicstate,
    )?;
    Ok(())
}

/// `render_string_horizontal` / `render_string_vertical`: identical but for the axis.
fn render_string_1d<D: PDFTextDevice + ?Sized>(
    dev: &mut D,
    vertical: bool,
    seq: &[Value],
    matrix: Matrix,
    pos: Point,
    font: &Rc<PDFFont>,
    fontsize: f64,
    scaling: f64,
    charspace: f64,
    wordspace: f64,
    rise: f64,
    dxscale: f64,
    ncs: &PDFColorSpace,
    graphicstate: &PDFGraphicStateRef,
) -> PyResult<Point> {
    let (mut x, mut y) = pos;
    // The coordinate that moves: y for vertical text, x otherwise.
    let advance = |x: &mut f64, y: &mut f64, d: f64| {
        if vertical {
            *y += d;
        } else {
            *x += d;
        }
    };
    let mut needcharspace = false;
    for obj in seq {
        if let Some(n) = obj.as_f64() {
            advance(&mut x, &mut y, -(n * dxscale));
            needcharspace = true;
        } else if let Value::Bytes(b) = obj {
            for cid in font.decode(b)? {
                if needcharspace {
                    advance(&mut x, &mut y, charspace);
                }
                let adv = dev.render_char(
                    utils::translate_matrix(matrix, (x, y)),
                    font,
                    fontsize,
                    scaling,
                    rise,
                    cid,
                    ncs,
                    graphicstate,
                )?;
                advance(&mut x, &mut y, adv);
                if cid == 32 && wordspace != 0.0 {
                    advance(&mut x, &mut y, wordspace);
                }
                needcharspace = true;
            }
        } else {
            warn!(
                "Cannot render {} string because {} is not a valid int, float or bytes.",
                if vertical { "vertical" } else { "horizontal" },
                obj.repr()
            );
        }
    }
    Ok((x, y))
}

/// `PDFTextDevice.render_string_horizontal(...)`.
pub fn render_string_horizontal<D: PDFTextDevice + ?Sized>(
    dev: &mut D,
    seq: &[Value],
    matrix: Matrix,
    pos: Point,
    font: &Rc<PDFFont>,
    fontsize: f64,
    scaling: f64,
    charspace: f64,
    wordspace: f64,
    rise: f64,
    dxscale: f64,
    ncs: &PDFColorSpace,
    graphicstate: &PDFGraphicStateRef,
) -> PyResult<Point> {
    render_string_1d(
        dev,
        false,
        seq,
        matrix,
        pos,
        font,
        fontsize,
        scaling,
        charspace,
        wordspace,
        rise,
        dxscale,
        ncs,
        graphicstate,
    )
}

/// `PDFTextDevice.render_string_vertical(...)`.
pub fn render_string_vertical<D: PDFTextDevice + ?Sized>(
    dev: &mut D,
    seq: &[Value],
    matrix: Matrix,
    pos: Point,
    font: &Rc<PDFFont>,
    fontsize: f64,
    scaling: f64,
    charspace: f64,
    wordspace: f64,
    rise: f64,
    dxscale: f64,
    ncs: &PDFColorSpace,
    graphicstate: &PDFGraphicStateRef,
) -> PyResult<Point> {
    render_string_1d(
        dev,
        true,
        seq,
        matrix,
        pos,
        font,
        fontsize,
        scaling,
        charspace,
        wordspace,
        rise,
        dxscale,
        ncs,
        graphicstate,
    )
}

/// `utils.enc(tag.name)`: `""` for a `bytes` name.
fn enc_name(name: &NameType) -> String {
    match name {
        NameType::Str(s) => utils::enc(s),
        NameType::Bytes(_) => String::new(),
    }
}

/// `utils.make_compat_str(v)` for a tag property.
pub fn make_compat_str(v: &Value) -> String {
    match v {
        // DIVERGE(charset-normalizer)
        Value::Bytes(b) => utils::make_compat_str_bytes(b),
        other => other.py_str(),
    }
}

/// `TagExtractor(rsrcmgr, outfp, codec="utf-8")`: writes the text and the marked-content tags
/// of the pages to `outfp`, encoded with `codec` (a binary stream).
pub struct TagExtractor {
    pub ctm: Option<Matrix>,
    pub outfp: AnyIO,
    pub codec: String,
    pub pageno: i64,
    _stack: Vec<PSLiteral>,
}

impl TagExtractor {
    pub fn new(outfp: AnyIO, codec: &str) -> Self {
        TagExtractor { ctm: None, outfp, codec: codec.to_string(), pageno: 0, _stack: Vec::new() }
    }

    fn _write(&mut self, s: &PyStr) -> PyResult<()> {
        let b = encode_str(s, &self.codec)?;
        self.outfp.borrow_mut().write_bytes(&b)
    }

    fn write_str(&mut self, s: &str) -> PyResult<()> {
        self._write(&PyStr::from(s))
    }
}

impl PDFDevice for TagExtractor {
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
        _ncs: &PDFColorSpace,
        _graphicstate: &PDFGraphicStateRef,
    ) -> PyResult<()> {
        let Some(font) = textstate.font.clone() else {
            return Err(PyErr::new(ExcKind::AssertionError, ""));
        };
        let mut text = PyStr::new();
        for obj in seq {
            let bytes: Vec<u8> = match obj {
                Value::Str(s) => utils::make_compat_bytes(s),
                Value::Bytes(b) => b.to_vec(),
                _ => continue,
            };
            for cid in font.decode(&bytes)? {
                match font.to_unichr(cid) {
                    Ok(char) => text.push_pystr(&char),
                    Err(e) if e.is(ExcKind::PDFUnicodeNotDefined) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        let s = utils::enc_pystr(&text);
        self._write(&s)
    }

    fn begin_page(&mut self, page: &PDFPage, _ctm: Matrix) -> PyResult<()> {
        let output = format!(
            "<page id=\"{}\" bbox=\"{}\" rotate=\"{}\">",
            self.pageno,
            utils::bbox2str(page.mediabox),
            page.rotate
        );
        self.write_str(&output)
    }

    fn end_page(&mut self, _page: &PDFPage) -> PyResult<()> {
        self.write_str("</page>\n")?;
        self.pageno += 1;
        Ok(())
    }

    fn begin_tag(&mut self, tag: &PSLiteral, props: Option<&Value>) -> PyResult<()> {
        let mut s = String::new();
        if let Some(Value::Dict(d)) = props {
            // sorted(props.items()): the keys are distinct strs, so they decide the order.
            let mut items: Vec<(Rc<str>, Value)> = d.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            items.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, v) in items {
                s.push_str(&format!(" {}=\"{}\"", utils::enc(&k), make_compat_str(&v)));
            }
        }
        let out_s = format!("<{}{}>", enc_name(&tag.name), s);
        self.write_str(&out_s)?;
        self._stack.push(tag.clone());
        Ok(())
    }

    fn end_tag(&mut self) -> PyResult<()> {
        let Some(tag) = self._stack.pop() else {
            return Err(PyErr::new(ExcKind::AssertionError, self.pageno.to_string()));
        };
        let out_s = format!("</{}>", enc_name(&tag.name));
        self.write_str(&out_s)
    }

    fn do_tag(&mut self, tag: &PSLiteral, props: Option<&Value>) -> PyResult<()> {
        self.begin_tag(tag, props)?;
        self._stack.pop();
        Ok(())
    }
}
