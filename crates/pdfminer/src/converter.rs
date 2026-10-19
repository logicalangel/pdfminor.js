//! Port of `pdfminer/converter.py`.
//!
//! Python's class hierarchy (`PDFTextDevice` <- `PDFLayoutAnalyzer` <- `PDFPageAggregator`,
//! `PDFConverter` <- `TextConverter`, `HTMLConverter`, `XMLConverter`, `HOCRConverter`) becomes:
//! the analyzer's state and inherited methods in [`PDFLayoutAnalyzer`], which every converter
//! embeds, and the methods the subclasses override (`receive_layout`, `render_image`,
//! `paint_path`, `close`) in the [`LayoutAnalyzerDevice`] trait. [`impl_layout_device!`] makes
//! each of them a [`PDFDevice`].
//!
//! Output goes to an [`AnyIO`] stream (Python's file object): `str` for a text stream, `bytes`
//! encoded with the converter's codec for a binary stream, exactly as Python decides (see
//! `pyrt::textio`).
//!
//! Numbers that Python prints with `str()` (`HTMLConverter`'s coordinates times `scale`, XML's
//! `linewidth`, `rotate`, image sizes, hOCR font sizes) keep Python's int/float distinction: the
//! page's `x0`/`y0` are the int `0` (`PDFLayoutAnalyzer.begin_page` builds the page bbox as
//! `(0, 0, w, h)`), and `HTMLConverter.scale` defaults to the int `1`.
//!
//! The render functions recurse like Python's; frames are counted from
//! [`ANALYZE_FRAME_DEPTH`] (`receive_layout` runs at the depth of `analyze`), so a group tree too
//! deep for Python raises `RecursionError` here too. DIVERGE(recursion-depth).

use std::rc::Rc;

use indexmap::IndexMap;

use crate::image::ImageWriter;
use crate::layout::{
    ANALYZE_FRAME_DEPTH, LAParams, LTChar, LTCurve, LTFigure, LTImage, LTItem, LTLine, LTPage, LTRect,
    PY_RECURSION_LIMIT,
};
use crate::pdfcolor::PDFColorSpace;
use crate::pdfdevice::{PDFTextDevice, make_compat_str, text_render_string};
use crate::pdffont::PDFFont;
use crate::pdfinterp::{
    FLOAT_MATRIX, MatrixInts, PDFGraphicStateRef, PDFResourceManager, PDFTextState, mult_matrix_value,
};
use crate::pdfpage::PDFPage;
use crate::pdftypes::PDFStream;
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::{fixed, float_repr};
use crate::pyrt::pystr::PyStr;
use crate::pyrt::textio::{AnyIO, Codec, Errors, encode, lookup};
use crate::pyrt::value::{Value, float_to_int, py_add, py_eq, py_mul};
use crate::utils::{Matrix, PathSegment, Point, Rect, apply_matrix_pt, apply_matrix_rect, bbox2str, enc, enc_pystr};

/// `RecursionError` when a Python frame at `depth` would exceed the recursion limit.
fn enter_frame(depth: usize) -> PyResult<()> {
    if depth > PY_RECURSION_LIMIT {
        return Err(PyErr::new(ExcKind::RecursionError, "maximum recursion depth exceeded"));
    }
    Ok(())
}

/// `str(type(obj))` of a layout object.
fn type_str(item: &LTItem) -> String {
    format!("<class 'pdfminer.layout.{}'>", item.class_name())
}

fn assertion(msg: impl Into<String>) -> PyErr {
    PyErr::new(ExcKind::AssertionError, msg)
}

/// A Python number that is printed with `str()`: an int stays an int.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn f(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }

    /// `str(x)`.
    pub fn py_str(self) -> String {
        match self {
            Num::Int(i) => i.to_string(),
            Num::Float(f) => float_repr(f),
        }
    }
}

/// Python's arithmetic: int op int stays an int (a float if it leaves `i64`), else floats.
macro_rules! num_op {
    ($trait:ident, $method:ident, $checked:ident, $op:tt) => {
        impl std::ops::$trait for Num {
            type Output = Num;
            fn $method(self, o: Num) -> Num {
                match (self, o) {
                    (Num::Int(a), Num::Int(b)) => a.$checked(b).map_or(Num::Float(a as f64 $op b as f64), Num::Int),
                    _ => Num::Float(self.f() $op o.f()),
                }
            }
        }
    };
}
num_op!(Add, add, checked_add, +);
num_op!(Sub, sub, checked_sub, -);
num_op!(Mul, mul, checked_mul, *);

/// `utils.enc(x)` of an untyped value: `""` for bytes, `html.escape(x)` for a `str`, and the
/// `AttributeError` of `x.replace` for anything else.
pub fn enc_value(v: &Value) -> PyResult<String> {
    match v {
        Value::Bytes(_) => Ok(String::new()),
        Value::Str(s) => Ok(enc(s)),
        other => Err(PyErr::new(
            ExcKind::AttributeError,
            format!("'{}' object has no attribute 'replace'", other.type_name()),
        )),
    }
}

/// `needle in haystack` for a `str` needle.
fn py_contains_str(haystack: &Value, needle: &str) -> PyResult<bool> {
    match haystack {
        Value::Str(s) => Ok(s.contains(needle)),
        Value::Bytes(_) => Err(PyErr::new(ExcKind::TypeError, "a bytes-like object is required, not 'str'")),
        Value::List(l) => Ok(l.borrow().iter().any(|x| py_eq(x, &Value::str(needle)))),
        Value::Tuple(t) => Ok(t.iter().any(|x| py_eq(x, &Value::str(needle)))),
        Value::Dict(d) => Ok(d.borrow().contains_key(needle)),
        Value::Stream(s) => Ok(s.contains(needle)),
        other => Err(PyErr::new(
            ExcKind::TypeError,
            format!("argument of type '{}' is not a container or iterable", other.type_name()),
        )),
    }
}

/// The `ValueError` of unpacking `got` items into `expected` names.
fn unpack_error(expected: usize, got: usize) -> PyErr {
    let msg = if got < expected {
        format!("not enough values to unpack (expected {expected}, got {got})")
    } else {
        format!("too many values to unpack (expected {expected}, got {got})")
    };
    PyErr::new(ExcKind::ValueError, msg)
}

// ---------------------------------------------------------------------------------------------
// PDFLayoutAnalyzer

/// `PDFLayoutAnalyzer`: builds the layout objects of a page from what the interpreter draws.
pub struct PDFLayoutAnalyzer {
    pub rsrcmgr: Rc<PDFResourceManager>,
    /// `self.ctm` (`None` until the interpreter sets it).
    pub ctm: Option<Matrix>,
    /// Which entries of `ctm` are Python ints.
    pub ctm_ints: MatrixInts,
    pub pageno: i64,
    pub laparams: Option<LAParams>,
    pub _stack: Vec<LTItem>,
    /// `self.cur_item` (unset until `begin_page`).
    pub cur_item: Option<LTItem>,
}

impl PDFLayoutAnalyzer {
    /// `PDFLayoutAnalyzer(rsrcmgr, pageno=1, laparams=None)`.
    pub fn new(rsrcmgr: Rc<PDFResourceManager>, pageno: i64, laparams: Option<LAParams>) -> Self {
        PDFLayoutAnalyzer {
            rsrcmgr,
            ctm: None,
            ctm_ints: FLOAT_MATRIX,
            pageno,
            laparams,
            _stack: Vec::new(),
            cur_item: None,
        }
    }

    fn cur(&self) -> PyResult<&LTItem> {
        self.cur_item.as_ref().ok_or_else(|| {
            PyErr::new(ExcKind::AttributeError, "'PDFLayoutAnalyzer' object has no attribute 'cur_item'")
        })
    }

    fn ctm_or_err(&self) -> PyResult<Matrix> {
        self.ctm.ok_or_else(|| PyErr::new(ExcKind::TypeError, "cannot unpack non-iterable NoneType object"))
    }

    pub fn begin_page(&mut self, page: &PDFPage, ctm: Matrix) -> PyResult<()> {
        self.begin_page_mediabox(page.mediabox, ctm)
    }

    /// `begin_page` given the page's `mediabox` (the only attribute it reads).
    pub fn begin_page_mediabox(&mut self, mediabox: Rect, ctm: Matrix) -> PyResult<()> {
        let (x0, y0, x1, y1) = apply_matrix_rect(ctm, mediabox);
        let mediabox = (0.0, 0.0, (x0 - x1).abs(), (y0 - y1).abs());
        self.cur_item = Some(LTPage(self.pageno, mediabox, Value::Int(0)));
        Ok(())
    }

    /// `end_page(page)` up to `receive_layout`: returns the page to receive.
    pub fn end_page(&mut self) -> PyResult<LTItem> {
        if !self._stack.is_empty() {
            return Err(assertion(self._stack.len().to_string()));
        }
        let cur = self.cur()?.clone();
        if !cur.is_ltpage() {
            return Err(assertion(type_str(&cur)));
        }
        if let Some(laparams) = &self.laparams {
            cur.analyze(laparams)?;
        }
        self.pageno += 1;
        Ok(cur)
    }

    /// `begin_figure(name, bbox, matrix)`: `bbox` and `matrix` are the untyped operands the
    /// interpreter passes.
    pub fn begin_figure(&mut self, name: &str, bbox: &[Value], matrix: &[Value]) -> PyResult<()> {
        let cur = self.cur()?.clone();
        self._stack.push(cur);
        // mult_matrix(matrix, self.ctm)
        if matrix.len() != 6 {
            return Err(unpack_error(6, matrix.len()));
        }
        let (m, _) = mult_matrix_value(matrix, self.ctm_or_err()?, self.ctm_ints)?;
        // LTFigure(name, bbox, m): (x, y, w, h) = bbox; rect = (x, y, x + w, y + h);
        // bbox = apply_matrix_rect(m, rect), with Python's arithmetic on the operands.
        if bbox.len() != 4 {
            return Err(unpack_error(4, bbox.len()));
        }
        let (x, y, w, h) = (&bbox[0], &bbox[1], &bbox[2], &bbox[3]);
        let rect = [x.clone(), y.clone(), py_add(x, w)?, py_add(y, h)?];
        let mut r = [0.0; 4];
        for (i, v) in rect.iter().enumerate() {
            r[i] = match v.as_f64() {
                Some(f) => f,
                // `a * x0` (a float times a non-number) raises first.
                None => return Err(py_mul(&Value::Float(m.0), v).unwrap_err()),
            };
        }
        let fig = LTFigure(name, (r[0], r[1], r[2] - r[0], r[3] - r[1]), m);
        // LTFigure recomputes x + w from floats; set the exact bbox.
        fig.set_bbox(apply_matrix_rect(m, (r[0], r[1], r[2], r[3])));
        self.cur_item = Some(fig);
        Ok(())
    }

    pub fn end_figure(&mut self, _name: &str) -> PyResult<()> {
        let fig = self.cur()?.clone();
        if !fig.is_ltfigure() {
            return Err(assertion(type_str(&fig)));
        }
        let Some(parent) = self._stack.pop() else {
            return Err(PyErr::new(ExcKind::IndexError, "pop from empty list"));
        };
        parent.add(fig);
        self.cur_item = Some(parent);
        Ok(())
    }

    pub fn render_image(&mut self, name: &str, stream: &Rc<PDFStream>) -> PyResult<()> {
        let cur = self.cur()?.clone();
        if !cur.is_ltfigure() {
            return Err(assertion(type_str(&cur)));
        }
        let item = LTImage(name, stream.clone(), (cur.x0(), cur.y0(), cur.x1(), cur.y1()));
        cur.add(item);
        Ok(())
    }

    /// Paint paths described in section 4.4 of the PDF reference manual
    pub fn paint_path(
        &mut self,
        gstate: &PDFGraphicStateRef,
        stroke: bool,
        fill: bool,
        evenodd: bool,
        path: &[PathSegment],
    ) -> PyResult<()> {
        let shape: String = path.iter().map(|x| x.op).collect();

        if !shape.starts_with('m') {
            // Per PDF Reference Section 4.4.1, "path construction operators may
            // be invoked in any sequence, but the first one invoked must be m
            // or re to begin a new subpath." Since pdfminer.six already
            // converts all `re` (rectangle) operators to their equivalent
            // `mlllh` representation, paths ingested by `.paint_path(...)` that
            // do not begin with the `m` operator are invalid.
        } else if shape.matches('m').count() > 1 {
            // recurse if there are multiple m's in this shape
            // (re.finditer(r"m[^m]+", shape))
            let ops: Vec<char> = shape.chars().collect();
            let mut i = 0;
            while i < ops.len() {
                if ops[i] == 'm' {
                    let mut j = i + 1;
                    while j < ops.len() && ops[j] != 'm' {
                        j += 1;
                    }
                    if j > i + 1 {
                        self.paint_path(gstate, stroke, fill, evenodd, &path[i..j])?;
                        i = j;
                        continue;
                    }
                }
                i += 1;
            }
        } else {
            let ctm = self.ctm_or_err()?;
            // Although the 'h' command does not not literally provide a
            // point-position, its position is (by definition) equal to the
            // subpath's starting point.
            //
            // And, per Section 4.4's Table 4.9, all other path commands place
            // their point-position in their final two arguments. (Any preceding
            // arguments represent control points on Bézier curves.)
            let last2 = |p: &PathSegment| -> PyResult<Point> {
                // `p[-2:]` of the tuple `(op, *args)`: with fewer than two operands it holds
                // the operator, which `apply_matrix_pt` cannot use (the interpreter never
                // builds such segments).
                match p.args.len() {
                    0 => Err(unpack_error(2, 1)),
                    1 => Err(PyErr::new(ExcKind::TypeError, "can't multiply sequence by non-int of type 'float'")),
                    n => Ok((p.args[n - 2], p.args[n - 1])),
                }
            };
            let mut raw_pts = Vec::with_capacity(path.len());
            for p in path {
                raw_pts.push(if p.op != 'h' { last2(p)? } else { last2(&path[0])? });
            }
            let mut pts: Vec<Point> = raw_pts.iter().map(|&pt| apply_matrix_pt(ctm, pt)).collect();

            let transformed_path: Vec<crate::layout::PathSegment> = path
                .iter()
                .map(|operation| {
                    let points =
                        operation.args.as_chunks::<2>().0.iter().map(|c| apply_matrix_pt(ctm, (c[0], c[1]))).collect();
                    (operation.op.to_string(), points)
                })
                .collect();

            let mut shape = shape;
            // Drop a redundant "l" on a path closed with "h"
            if shape.len() > 3 && shape.ends_with("lh") && pts[pts.len() - 2] == pts[0] {
                shape = format!("{}h", &shape[..shape.len() - 2]);
                pts.pop();
            }

            let gs = gstate.borrow();
            let (linewidth, scolor, ncolor, dash) =
                (gs.linewidth.clone(), Some(gs.scolor.clone()), Some(gs.ncolor.clone()), gs.dash.clone());
            drop(gs);
            let cur = self.cur()?.clone();

            if shape == "mlh" || shape == "ml" {
                // single line segment
                //
                // Note: 'ml', in conditional above, is a frequent anomaly
                // that we want to support.
                let line = LTLine(
                    linewidth,
                    pts[0],
                    pts[1],
                    stroke,
                    fill,
                    evenodd,
                    scolor,
                    ncolor,
                    Some(transformed_path),
                    dash,
                );
                cur.add(line);
            } else if shape == "mlllh" || shape == "mllll" {
                let ((x0, y0), (x1, y1), (x2, y2), (x3, y3)) = (pts[0], pts[1], pts[2], pts[3]);

                let is_closed_loop = pts[0] == pts[4];
                let has_square_coordinates =
                    (x0 == x1 && y1 == y2 && x2 == x3 && y3 == y0) || (y0 == y1 && x1 == x2 && y2 == y3 && x3 == x0);
                if is_closed_loop && has_square_coordinates {
                    let rect = LTRect(
                        linewidth,
                        (pts[0].0, pts[0].1, pts[2].0, pts[2].1),
                        stroke,
                        fill,
                        evenodd,
                        scolor,
                        ncolor,
                        Some(transformed_path),
                        dash,
                    );
                    cur.add(rect);
                } else {
                    let curve =
                        LTCurve(linewidth, pts, stroke, fill, evenodd, scolor, ncolor, Some(transformed_path), dash);
                    cur.add(curve);
                }
            } else {
                let curve =
                    LTCurve(linewidth, pts, stroke, fill, evenodd, scolor, ncolor, Some(transformed_path), dash);
                cur.add(curve);
            }
        }
        Ok(())
    }

    pub fn handle_undefined_char(&mut self, font: &PDFFont, cid: i64) -> PyStr {
        log::debug!("undefined: {}, {}", font.repr(), cid);
        PyStr::from(format!("(cid:{cid})"))
    }

    /// `PDFTextDevice.render_string` with this analyzer's `render_char`.
    pub fn render_string(
        &mut self,
        textstate: &mut PDFTextState,
        seq: &[Value],
        ncs: &PDFColorSpace,
        graphicstate: &PDFGraphicStateRef,
    ) -> PyResult<()> {
        text_render_string(self, textstate, seq, ncs, graphicstate)
    }
}

impl PDFTextDevice for PDFLayoutAnalyzer {
    fn render_char(
        &mut self,
        matrix: Matrix,
        font: &Rc<PDFFont>,
        fontsize: f64,
        scaling: f64,
        rise: f64,
        cid: i64,
        ncs: &PDFColorSpace,
        graphicstate: &PDFGraphicStateRef,
    ) -> PyResult<f64> {
        let text = match font.to_unichr(cid) {
            Ok(t) => t,
            Err(e) if e.is(ExcKind::PDFUnicodeNotDefined) => self.handle_undefined_char(font, cid),
            Err(e) => return Err(e),
        };
        let textwidth = font.char_width(cid)?;
        let textdisp = font.char_disp(cid)?;
        let item = LTChar(
            matrix,
            font,
            fontsize,
            scaling,
            rise,
            text,
            textwidth,
            &textdisp,
            ncs.clone(),
            graphicstate.clone(),
        )?;
        let adv = item.as_char().expect("LTChar").adv;
        self.cur()?.add(item);
        Ok(adv)
    }
}

/// The methods `PDFLayoutAnalyzer`'s subclasses override.
pub trait LayoutAnalyzerDevice {
    fn analyzer(&self) -> &PDFLayoutAnalyzer;
    fn analyzer_mut(&mut self) -> &mut PDFLayoutAnalyzer;

    fn receive_layout(&mut self, _ltpage: LTItem) -> PyResult<()> {
        Ok(())
    }

    fn render_image(&mut self, name: &str, stream: &Rc<PDFStream>) -> PyResult<()> {
        self.analyzer_mut().render_image(name, stream)
    }

    fn paint_path(
        &mut self,
        gstate: &PDFGraphicStateRef,
        stroke: bool,
        fill: bool,
        evenodd: bool,
        path: &[PathSegment],
    ) -> PyResult<()> {
        self.analyzer_mut().paint_path(gstate, stroke, fill, evenodd, path)
    }

    fn close(&mut self) -> PyResult<()> {
        Ok(())
    }
}

/// Implements [`PDFDevice`](crate::pdfdevice::PDFDevice) for a [`LayoutAnalyzerDevice`].
#[macro_export]
macro_rules! impl_layout_device {
    ($t:ty) => {
        impl $crate::pdfdevice::PDFDevice for $t {
            fn ctm(&self) -> Option<$crate::utils::Matrix> {
                $crate::converter::LayoutAnalyzerDevice::analyzer(self).ctm
            }
            fn set_ctm(&mut self, ctm: $crate::utils::Matrix) {
                self.set_ctm_py(ctm, $crate::pdfinterp::FLOAT_MATRIX);
            }
            fn set_ctm_py(&mut self, ctm: $crate::utils::Matrix, ints: $crate::pdfinterp::MatrixInts) {
                let analyzer = $crate::converter::LayoutAnalyzerDevice::analyzer_mut(self);
                analyzer.ctm = Some(ctm);
                analyzer.ctm_ints = ints;
            }
            fn close(&mut self) -> $crate::pyrt::error::PyResult<()> {
                $crate::converter::LayoutAnalyzerDevice::close(self)
            }
            fn begin_page(
                &mut self,
                page: &$crate::pdfpage::PDFPage,
                ctm: $crate::utils::Matrix,
            ) -> $crate::pyrt::error::PyResult<()> {
                $crate::converter::LayoutAnalyzerDevice::analyzer_mut(self).begin_page(page, ctm)
            }
            fn end_page(&mut self, _page: &$crate::pdfpage::PDFPage) -> $crate::pyrt::error::PyResult<()> {
                let ltpage = $crate::converter::LayoutAnalyzerDevice::analyzer_mut(self).end_page()?;
                $crate::converter::LayoutAnalyzerDevice::receive_layout(self, ltpage)
            }
            fn begin_figure(
                &mut self,
                name: &str,
                bbox: &[$crate::pyrt::value::Value],
                matrix: &[$crate::pyrt::value::Value],
            ) -> $crate::pyrt::error::PyResult<()> {
                $crate::converter::LayoutAnalyzerDevice::analyzer_mut(self).begin_figure(name, bbox, matrix)
            }
            fn end_figure(&mut self, name: &str) -> $crate::pyrt::error::PyResult<()> {
                $crate::converter::LayoutAnalyzerDevice::analyzer_mut(self).end_figure(name)
            }
            fn paint_path(
                &mut self,
                graphicstate: &$crate::pdfinterp::PDFGraphicStateRef,
                stroke: bool,
                fill: bool,
                evenodd: bool,
                path: &[$crate::utils::PathSegment],
            ) -> $crate::pyrt::error::PyResult<()> {
                $crate::converter::LayoutAnalyzerDevice::paint_path(self, graphicstate, stroke, fill, evenodd, path)
            }
            fn render_image(
                &mut self,
                name: &str,
                stream: &std::rc::Rc<$crate::pdftypes::PDFStream>,
            ) -> $crate::pyrt::error::PyResult<()> {
                $crate::converter::LayoutAnalyzerDevice::render_image(self, name, stream)
            }
            fn render_string(
                &mut self,
                textstate: &mut $crate::pdfinterp::PDFTextState,
                seq: &[$crate::pyrt::value::Value],
                ncs: &$crate::pdfcolor::PDFColorSpace,
                graphicstate: &$crate::pdfinterp::PDFGraphicStateRef,
            ) -> $crate::pyrt::error::PyResult<()> {
                $crate::converter::LayoutAnalyzerDevice::analyzer_mut(self).render_string(
                    textstate,
                    seq,
                    ncs,
                    graphicstate,
                )
            }
        }
    };
}

/// `PDFLayoutAnalyzer` itself as a device (its `receive_layout` does nothing).
impl LayoutAnalyzerDevice for PDFLayoutAnalyzer {
    fn analyzer(&self) -> &PDFLayoutAnalyzer {
        self
    }
    fn analyzer_mut(&mut self) -> &mut PDFLayoutAnalyzer {
        self
    }
}
impl_layout_device!(PDFLayoutAnalyzer);

// ---------------------------------------------------------------------------------------------
// PDFPageAggregator

pub struct PDFPageAggregator {
    pub analyzer: PDFLayoutAnalyzer,
    pub result: Option<LTItem>,
}

impl PDFPageAggregator {
    /// `PDFPageAggregator(rsrcmgr, pageno=1, laparams=None)`.
    pub fn new(rsrcmgr: Rc<PDFResourceManager>, pageno: i64, laparams: Option<LAParams>) -> Self {
        PDFPageAggregator { analyzer: PDFLayoutAnalyzer::new(rsrcmgr, pageno, laparams), result: None }
    }

    pub fn get_result(&self) -> PyResult<LTItem> {
        self.result.clone().ok_or_else(|| assertion(""))
    }
}

impl LayoutAnalyzerDevice for PDFPageAggregator {
    fn analyzer(&self) -> &PDFLayoutAnalyzer {
        &self.analyzer
    }
    fn analyzer_mut(&mut self) -> &mut PDFLayoutAnalyzer {
        &mut self.analyzer
    }
    fn receive_layout(&mut self, ltpage: LTItem) -> PyResult<()> {
        self.result = Some(ltpage);
        Ok(())
    }
}
impl_layout_device!(PDFPageAggregator);

// ---------------------------------------------------------------------------------------------
// PDFConverter

/// `PDFConverter`'s state: the output stream and codec (`""` is Python's falsy codec).
pub struct PDFConverter {
    pub outfp: AnyIO,
    pub codec: String,
    pub outfp_binary: bool,
    /// `codecs.lookup(codec)` (looked up once; Python looks it up on every write, with the same
    /// result).
    codec_impl: Option<PyResult<Codec>>,
}

impl PDFConverter {
    pub fn new(outfp: AnyIO, codec: &str) -> Self {
        let outfp_binary = Self::_is_binary_stream(&outfp);
        let codec_impl = if codec.is_empty() { None } else { Some(lookup(codec)) };
        PDFConverter { outfp, codec: codec.to_string(), outfp_binary, codec_impl }
    }

    /// Test if an stream is binary or not
    pub fn _is_binary_stream(outfp: &AnyIO) -> bool {
        outfp.borrow().is_binary()
    }

    /// `write(text)` of the HTML, XML and hOCR converters: `text.encode(self.codec)` to the
    /// stream when there is a codec, the `str` itself otherwise.
    fn write_codec(&self, text: &PyStr) -> PyResult<()> {
        match &self.codec_impl {
            Some(Ok(c)) => {
                let b = encode(text, *c, Errors::Strict)?;
                self.outfp.borrow_mut().write_bytes(&b)
            }
            Some(Err(e)) => Err(e.clone()),
            None => self.outfp.borrow_mut().write_str(text),
        }
    }

    fn write_codec_str(&self, text: &str) -> PyResult<()> {
        self.write_codec(&PyStr::from(text))
    }
}

// ---------------------------------------------------------------------------------------------
// TextConverter

pub struct TextConverter {
    pub analyzer: PDFLayoutAnalyzer,
    pub conv: PDFConverter,
    pub showpageno: bool,
    pub imagewriter: Option<ImageWriter>,
}

impl TextConverter {
    /// `TextConverter(rsrcmgr, outfp, codec="utf-8", pageno=1, laparams=None,
    /// showpageno=False, imagewriter=None)`.
    pub fn new(
        rsrcmgr: Rc<PDFResourceManager>,
        outfp: AnyIO,
        codec: &str,
        pageno: i64,
        laparams: Option<LAParams>,
        showpageno: bool,
        imagewriter: Option<ImageWriter>,
    ) -> Self {
        TextConverter {
            analyzer: PDFLayoutAnalyzer::new(rsrcmgr, pageno, laparams),
            conv: PDFConverter::new(outfp, codec),
            showpageno,
            imagewriter,
        }
    }

    pub fn write_text(&mut self, text: &PyStr) -> PyResult<()> {
        // utils.compatible_encode_method(text, self.codec, "ignore") returns a str as is.
        if self.conv.outfp_binary {
            let b = encode(text, Codec::Utf8, Errors::Strict)?;
            self.conv.outfp.borrow_mut().write_bytes(&b)
        } else {
            self.conv.outfp.borrow_mut().write_str(text)
        }
    }

    fn render(&mut self, item: &LTItem, depth: usize) -> PyResult<()> {
        enter_frame(depth)?;
        if item.is_ltcontainer() {
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
        } else if item.is_lttext() {
            self.write_text(&item.get_text())?;
        }
        if item.is_lttextbox() {
            self.write_text(&PyStr::from("\n"))?;
        } else if item.is_ltimage()
            && let Some(w) = &self.imagewriter
        {
            w.export_image(item)?;
        }
        Ok(())
    }
}

impl LayoutAnalyzerDevice for TextConverter {
    fn analyzer(&self) -> &PDFLayoutAnalyzer {
        &self.analyzer
    }
    fn analyzer_mut(&mut self) -> &mut PDFLayoutAnalyzer {
        &mut self.analyzer
    }

    fn receive_layout(&mut self, ltpage: LTItem) -> PyResult<()> {
        if self.showpageno {
            let pageid = ltpage.as_page().expect("LTPage").pageid;
            self.write_text(&PyStr::from(format!("Page {pageid}\n")))?;
        }
        self.render(&ltpage, ANALYZE_FRAME_DEPTH + 1)?;
        self.write_text(&PyStr::from("\x0c"))
    }

    // Some dummy functions to save memory/CPU when all that is wanted
    // is text.  This stops all the image and drawing output from being
    // recorded and taking up RAM.
    fn render_image(&mut self, name: &str, stream: &Rc<PDFStream>) -> PyResult<()> {
        if self.imagewriter.is_some() {
            self.analyzer.render_image(name, stream)?;
        }
        Ok(())
    }

    fn paint_path(
        &mut self,
        _gstate: &PDFGraphicStateRef,
        _stroke: bool,
        _fill: bool,
        _evenodd: bool,
        _path: &[PathSegment],
    ) -> PyResult<()> {
        Ok(())
    }
}
impl_layout_device!(TextConverter);

// ---------------------------------------------------------------------------------------------
// HTMLConverter

/// The keyword arguments of `HTMLConverter(...)`, with Python's defaults.
pub struct HTMLConverterParams {
    pub codec: String,
    pub pageno: i64,
    pub laparams: Option<LAParams>,
    pub scale: Num,
    pub fontscale: Num,
    pub layoutmode: String,
    pub showpageno: bool,
    pub pagemargin: i64,
    pub imagewriter: Option<ImageWriter>,
    pub debug: i64,
    pub rect_colors: Option<IndexMap<String, String>>,
    pub text_colors: Option<IndexMap<String, String>>,
}

impl Default for HTMLConverterParams {
    fn default() -> Self {
        HTMLConverterParams {
            codec: "utf-8".into(),
            pageno: 1,
            laparams: None,
            scale: Num::Int(1),
            fontscale: Num::Float(1.0),
            layoutmode: "normal".into(),
            showpageno: true,
            pagemargin: 50,
            imagewriter: None,
            debug: 0,
            rect_colors: None,
            text_colors: None,
        }
    }
}

pub struct HTMLConverter {
    pub analyzer: PDFLayoutAnalyzer,
    pub conv: PDFConverter,
    pub scale: Num,
    pub fontscale: Num,
    pub layoutmode: String,
    pub showpageno: bool,
    pub pagemargin: i64,
    pub imagewriter: Option<ImageWriter>,
    pub rect_colors: IndexMap<String, String>,
    pub text_colors: IndexMap<String, String>,
    pub _yoffset: Num,
    pub _font: Option<(String, f64)>,
    pub _fontstack: Vec<Option<(String, f64)>>,
}

impl HTMLConverter {
    pub const RECT_COLORS: [(&'static str, &'static str); 6] = [
        ("figure", "yellow"),
        ("textline", "magenta"),
        ("textbox", "cyan"),
        ("textgroup", "red"),
        ("curve", "black"),
        ("page", "gray"),
    ];

    pub const TEXT_COLORS: [(&'static str, &'static str); 2] = [("textbox", "blue"), ("char", "black")];

    pub fn new(rsrcmgr: Rc<PDFResourceManager>, outfp: AnyIO, p: HTMLConverterParams) -> PyResult<Self> {
        let conv = PDFConverter::new(outfp, &p.codec);

        // write() assumes a codec for binary I/O, or no codec for text I/O.
        if conv.outfp_binary && conv.codec.is_empty() {
            return Err(PyErr::new(ExcKind::PDFValueError, "Codec is required for a binary I/O output"));
        }
        if !conv.outfp_binary && !conv.codec.is_empty() {
            return Err(PyErr::new(ExcKind::PDFValueError, "Codec must not be specified for a text I/O output"));
        }

        let mut text_colors = p.text_colors.unwrap_or_else(|| [("char".to_string(), "black".to_string())].into());
        let mut rect_colors = p.rect_colors.unwrap_or_else(|| {
            [("curve".to_string(), "black".to_string()), ("page".to_string(), "gray".to_string())].into()
        });
        if p.debug != 0 {
            for (k, v) in Self::RECT_COLORS {
                rect_colors.insert(k.into(), v.into());
            }
            for (k, v) in Self::TEXT_COLORS {
                text_colors.insert(k.into(), v.into());
            }
        }
        let mut c = HTMLConverter {
            analyzer: PDFLayoutAnalyzer::new(rsrcmgr, p.pageno, p.laparams),
            conv,
            scale: p.scale,
            fontscale: p.fontscale,
            layoutmode: p.layoutmode,
            showpageno: p.showpageno,
            pagemargin: p.pagemargin,
            imagewriter: p.imagewriter,
            rect_colors,
            text_colors,
            _yoffset: Num::Int(p.pagemargin),
            _font: None,
            _fontstack: Vec::new(),
        };
        c.write_header()?;
        Ok(c)
    }

    pub fn write(&self, text: &str) -> PyResult<()> {
        self.conv.write_codec_str(text)
    }

    pub fn write_header(&mut self) -> PyResult<()> {
        self.write("<html><head>\n")?;
        let s = if !self.conv.codec.is_empty() {
            format!("<meta http-equiv=\"Content-Type\" content=\"text/html; charset={}\">\n", self.conv.codec)
        } else {
            "<meta http-equiv=\"Content-Type\" content=\"text/html\">\n".to_string()
        };
        self.write(&s)?;
        self.write("</head><body>\n")
    }

    pub fn write_footer(&mut self) -> PyResult<()> {
        let page_links: Vec<String> = (1..self.analyzer.pageno).map(|i| format!("<a href=\"#{i}\">{i}</a>")).collect();
        let s = format!("<div style=\"position:absolute; top:0px;\">Page: {}</div>\n", page_links.join(", "));
        self.write(&s)?;
        self.write("</body></html>\n")
    }

    pub fn write_text(&self, text: &PyStr) -> PyResult<()> {
        self.conv.write_codec(&enc_pystr(text))
    }

    pub fn place_rect(&self, color: &str, borderwidth: i64, x: Num, y: Num, w: Num, h: Num) -> PyResult<()> {
        if let Some(color2) = self.rect_colors.get(color) {
            let s = format!(
                "<span style=\"position:absolute; border: {} {}px solid; left:{}px; top:{}px; width:{}px; height:{}px;\"></span>\n",
                color2,
                borderwidth,
                (x * self.scale).py_str(),
                ((self._yoffset - y) * self.scale).py_str(),
                (w * self.scale).py_str(),
                (h * self.scale).py_str()
            );
            self.write(&s)?;
        }
        Ok(())
    }

    pub fn place_border(&self, color: &str, borderwidth: i64, item: &LTItem) -> PyResult<()> {
        // A page's x0 is Python's int 0.
        let x0 = if item.is_ltpage() && item.x0() == 0.0 { Num::Int(0) } else { Num::Float(item.x0()) };
        self.place_rect(
            color,
            borderwidth,
            x0,
            Num::Float(item.y1()),
            Num::Float(item.width()),
            Num::Float(item.height()),
        )
    }

    pub fn place_image(&self, item: &LTItem, borderwidth: i64, x: Num, y: Num, w: Num, h: Num) -> PyResult<()> {
        if let Some(writer) = &self.imagewriter {
            let name = writer.export_image(item)?;
            let s = format!(
                "<img src=\"{}\" border=\"{}\" style=\"position:absolute; left:{}px; top:{}px;\" width=\"{}\" height=\"{}\" />\n",
                enc(&name),
                borderwidth,
                (x * self.scale).py_str(),
                ((self._yoffset - y) * self.scale).py_str(),
                (w * self.scale).py_str(),
                (h * self.scale).py_str()
            );
            self.write(&s)?;
        }
        Ok(())
    }

    pub fn place_text(&self, color: &str, text: &PyStr, x: Num, y: Num, size: Num) -> PyResult<()> {
        if let Some(color2) = self.text_colors.get(color) {
            let s = format!(
                "<span style=\"position:absolute; color:{}; left:{}px; top:{}px; font-size:{}px;\">",
                color2,
                (x * self.scale).py_str(),
                ((self._yoffset - y) * self.scale).py_str(),
                (size * self.scale * self.fontscale).py_str()
            );
            self.write(&s)?;
            self.write_text(text)?;
            self.write("</span>\n")?;
        }
        Ok(())
    }

    pub fn begin_div(
        &mut self,
        color: &str,
        borderwidth: i64,
        x: Num,
        y: Num,
        w: Num,
        h: Num,
        writing_mode: &str,
    ) -> PyResult<()> {
        self._fontstack.push(self._font.take());
        let s = format!(
            "<div style=\"position:absolute; border: {} {}px solid; writing-mode:{}; left:{}px; top:{}px; width:{}px; height:{}px;\">",
            color,
            borderwidth,
            writing_mode,
            (x * self.scale).py_str(),
            ((self._yoffset - y) * self.scale).py_str(),
            (w * self.scale).py_str(),
            (h * self.scale).py_str()
        );
        self.write(&s)
    }

    pub fn end_div(&mut self, _color: &str) -> PyResult<()> {
        if self._font.is_some() {
            self.write("</span>")?;
        }
        self._font = match self._fontstack.pop() {
            Some(f) => f,
            None => return Err(PyErr::new(ExcKind::IndexError, "pop from empty list")),
        };
        self.write("</div>")
    }

    pub fn put_text(&mut self, text: &PyStr, fontname: &str, fontsize: f64) -> PyResult<()> {
        #[allow(clippy::float_cmp)]
        let same = matches!(&self._font, Some((n, s)) if n == fontname && *s == fontsize);
        if !same {
            if self._font.is_some() {
                self.write("</span>")?;
            }
            // Remove subset tag from fontname, see PDF Reference 5.5.3
            let fontname_without_subset_tag = fontname.rsplit('+').next().unwrap_or("");
            self.write(&format!(
                "<span style=\"font-family: {}; font-size:{}px\">",
                fontname_without_subset_tag,
                (Num::Float(fontsize) * self.scale * self.fontscale).py_str()
            ))?;
            self._font = Some((fontname.to_string(), fontsize));
        }
        self.write_text(text)
    }

    pub fn put_newline(&mut self) -> PyResult<()> {
        self.write("<br>")
    }

    fn show_group(&mut self, item: &LTItem, depth: usize) -> PyResult<()> {
        enter_frame(depth)?;
        if item.is_lttextgroup() {
            self.place_border("textgroup", 1, item)?;
            for child in item.objs() {
                self.show_group(&child, depth + 1)?;
            }
        }
        Ok(())
    }

    fn render(&mut self, item: &LTItem, depth: usize) -> PyResult<()> {
        enter_frame(depth)?;
        let (x0, y1, w, h) =
            (Num::Float(item.x0()), Num::Float(item.y1()), Num::Float(item.width()), Num::Float(item.height()));
        if item.is_ltpage() {
            let pageid = item.as_page().expect("LTPage").pageid;
            self._yoffset = self._yoffset + y1;
            self.place_border("page", 1, item)?;
            if self.showpageno {
                self.write(&format!(
                    "<div style=\"position:absolute; top:%dpx;\">{}",
                    ((self._yoffset - y1) * self.scale).py_str()
                ))?;
                self.write(&format!("<a name=\"{pageid}\">Page {pageid}</a></div>\n"))?;
            }
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
            if let Some(groups) = item.groups() {
                for group in groups {
                    self.show_group(&group, depth + 1)?;
                }
            }
        } else if item.is_ltcurve() {
            self.place_border("curve", 1, item)?;
        } else if item.is_ltfigure() {
            self.begin_div("figure", 1, x0, y1, w, h, "False")?;
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
            self.end_div("figure")?;
        } else if item.is_ltimage() {
            self.place_image(item, 1, x0, y1, w, h)?;
        } else if self.layoutmode == "exact" {
            if item.is_lttextline() {
                self.place_border("textline", 1, item)?;
                for child in item.objs() {
                    self.render(&child, depth + 1)?;
                }
            } else if item.is_lttextbox() {
                self.place_border("textbox", 1, item)?;
                self.place_text("textbox", &PyStr::from((item.index() + 1).to_string()), x0, y1, Num::Int(20))?;
                for child in item.objs() {
                    self.render(&child, depth + 1)?;
                }
            } else if item.is_ltchar() {
                self.place_border("char", 1, item)?;
                let size = item.as_char().expect("LTChar").size;
                self.place_text("char", &item.get_text(), x0, y1, Num::Float(size))?;
            }
        } else if item.is_lttextline() {
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
            if self.layoutmode != "loose" {
                self.put_newline()?;
            }
        } else if item.is_lttextbox() {
            let wmode = item.get_writing_mode()?;
            self.begin_div("textbox", 1, x0, y1, w, h, wmode)?;
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
            self.end_div("textbox")?;
        } else if item.is_ltchar() {
            let c = item.as_char().expect("LTChar");
            let fontname = make_compat_str(&c.fontname);
            let size = c.size;
            self.put_text(&item.get_text(), &fontname, size)?;
        } else if item.is_lttext() {
            self.write_text(&item.get_text())?;
        }
        Ok(())
    }
}

impl LayoutAnalyzerDevice for HTMLConverter {
    fn analyzer(&self) -> &PDFLayoutAnalyzer {
        &self.analyzer
    }
    fn analyzer_mut(&mut self) -> &mut PDFLayoutAnalyzer {
        &mut self.analyzer
    }
    fn receive_layout(&mut self, ltpage: LTItem) -> PyResult<()> {
        self.render(&ltpage, ANALYZE_FRAME_DEPTH + 1)?;
        self._yoffset = self._yoffset + Num::Int(self.pagemargin);
        Ok(())
    }
    fn close(&mut self) -> PyResult<()> {
        self.write_footer()
    }
}
impl_layout_device!(HTMLConverter);

// ---------------------------------------------------------------------------------------------
// XMLConverter

/// `CONTROL.sub("", text)`: removes `[\x00-\x08\x0b-\x0c\x0e-\x1f]`.
fn strip_control(text: &PyStr) -> PyStr {
    PyStr::from_code_points(text.code_points().filter(|&c| !matches!(c, 0x00..=0x08 | 0x0b..=0x0c | 0x0e..=0x1f)))
}

pub struct XMLConverter {
    pub analyzer: PDFLayoutAnalyzer,
    pub conv: PDFConverter,
    pub imagewriter: Option<ImageWriter>,
    pub stripcontrol: bool,
}

impl XMLConverter {
    /// `XMLConverter(rsrcmgr, outfp, codec="utf-8", pageno=1, laparams=None,
    /// imagewriter=None, stripcontrol=False)`.
    pub fn new(
        rsrcmgr: Rc<PDFResourceManager>,
        outfp: AnyIO,
        codec: &str,
        pageno: i64,
        laparams: Option<LAParams>,
        imagewriter: Option<ImageWriter>,
        stripcontrol: bool,
    ) -> PyResult<Self> {
        let conv = PDFConverter::new(outfp, codec);

        // write() assumes a codec for binary I/O, or no codec for text I/O.
        if conv.outfp_binary == conv.codec.is_empty() {
            return Err(PyErr::new(ExcKind::PDFValueError, "Codec is required for a binary I/O output"));
        }

        let mut c = XMLConverter {
            analyzer: PDFLayoutAnalyzer::new(rsrcmgr, pageno, laparams),
            conv,
            imagewriter,
            stripcontrol,
        };
        c.write_header()?;
        Ok(c)
    }

    pub fn write(&self, text: &str) -> PyResult<()> {
        self.conv.write_codec_str(text)
    }

    pub fn write_header(&mut self) -> PyResult<()> {
        if !self.conv.codec.is_empty() {
            self.write(&format!("<?xml version=\"1.0\" encoding=\"{}\" ?>\n", self.conv.codec))?;
        } else {
            self.write("<?xml version=\"1.0\" ?>\n")?;
        }
        self.write("<pages>\n")
    }

    pub fn write_footer(&mut self) -> PyResult<()> {
        self.write("</pages>\n")
    }

    pub fn write_text(&self, text: &PyStr) -> PyResult<()> {
        let text = if self.stripcontrol { strip_control(text) } else { text.clone() };
        self.conv.write_codec(&enc_pystr(&text))
    }

    fn show_group(&self, item: &LTItem, depth: usize) -> PyResult<()> {
        enter_frame(depth)?;
        if item.is_lttextbox() {
            self.write(&format!("<textbox id=\"{}\" bbox=\"{}\" />\n", item.index(), bbox2str(item.bbox())))?;
        } else if item.is_lttextgroup() {
            self.write(&format!("<textgroup bbox=\"{}\">\n", bbox2str(item.bbox())))?;
            for child in item.objs() {
                self.show_group(&child, depth + 1)?;
            }
            self.write("</textgroup>\n")?;
        }
        Ok(())
    }

    fn render(&self, item: &LTItem, depth: usize) -> PyResult<()> {
        enter_frame(depth)?;
        if item.is_ltpage() {
            let p = item.as_page().expect("LTPage");
            let s = format!(
                "<page id=\"{}\" bbox=\"{}\" rotate=\"{}\">\n",
                p.pageid,
                bbox2str(item.bbox()),
                p.rotate.py_str()
            );
            self.write(&s)?;
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
            if let Some(groups) = item.groups() {
                self.write("<layout>\n")?;
                for group in groups {
                    self.show_group(&group, depth + 1)?;
                }
                self.write("</layout>\n")?;
            }
            self.write("</page>\n")?;
        } else if item.is_ltline() {
            let c = item.as_curve().expect("LTLine");
            self.write(&format!(
                "<line linewidth=\"{}\" bbox=\"{}\" />\n",
                c.linewidth.py_str(),
                bbox2str(item.bbox())
            ))?;
        } else if item.is_ltrect() {
            let c = item.as_curve().expect("LTRect");
            self.write(&format!(
                "<rect linewidth=\"{}\" bbox=\"{}\" />\n",
                c.linewidth.py_str(),
                bbox2str(item.bbox())
            ))?;
        } else if item.is_ltcurve() {
            let c = item.as_curve().expect("LTCurve");
            self.write(&format!(
                "<curve linewidth=\"{}\" bbox=\"{}\" pts=\"{}\"/>\n",
                c.linewidth.py_str(),
                bbox2str(item.bbox()),
                c.get_pts()
            ))?;
        } else if item.is_ltfigure() {
            let f = item.as_figure().expect("LTFigure");
            self.write(&format!("<figure name=\"{}\" bbox=\"{}\">\n", f.name, bbox2str(item.bbox())))?;
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
            self.write("</figure>\n")?;
        } else if item.is_lttextline() {
            self.write(&format!("<textline bbox=\"{}\">\n", bbox2str(item.bbox())))?;
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
            self.write("</textline>\n")?;
        } else if item.is_lttextbox() {
            let wmode = if item.is_lttextboxvertical() { " wmode=\"vertical\"" } else { "" };
            self.write(&format!("<textbox id=\"{}\" bbox=\"{}\"{}>\n", item.index(), bbox2str(item.bbox()), wmode))?;
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
            self.write("</textbox>\n")?;
        } else if item.is_ltchar() {
            let c = item.as_char().expect("LTChar");
            let ncolour = c.graphicstate.borrow().ncolor.py_str();
            let s = format!(
                "<text font=\"{}\" bbox=\"{}\" colourspace=\"{}\" ncolour=\"{}\" size=\"{}\">",
                enc_value(&c.fontname)?,
                bbox2str(item.bbox()),
                c.ncs.name(),
                ncolour,
                fixed(c.size, 3)
            );
            self.write(&s)?;
            self.write_text(&item.get_text())?;
            self.write("</text>\n")?;
        } else if item.is_lttext() {
            let mut s = PyStr::from("<text>");
            s.push_pystr(&item.get_text());
            s.push_str("</text>\n");
            self.conv.write_codec(&s)?;
        } else if item.is_ltimage() {
            if let Some(writer) = &self.imagewriter {
                let name = writer.export_image(item)?;
                self.write(&format!(
                    "<image src=\"{}\" width=\"{}\" height=\"{}\" />\n",
                    enc(&name),
                    float_repr(item.width()),
                    float_repr(item.height())
                ))?;
            } else {
                self.write(&format!(
                    "<image width=\"{}\" height=\"{}\" />\n",
                    float_repr(item.width()),
                    float_repr(item.height())
                ))?;
            }
        } else {
            return Err(assertion(format!("('Unhandled', {})", item.repr())));
        }
        Ok(())
    }
}

impl LayoutAnalyzerDevice for XMLConverter {
    fn analyzer(&self) -> &PDFLayoutAnalyzer {
        &self.analyzer
    }
    fn analyzer_mut(&mut self) -> &mut PDFLayoutAnalyzer {
        &mut self.analyzer
    }
    fn receive_layout(&mut self, ltpage: LTItem) -> PyResult<()> {
        self.render(&ltpage, ANALYZE_FRAME_DEPTH + 1)
    }
    fn close(&mut self) -> PyResult<()> {
        self.write_footer()
    }
}
impl_layout_device!(XMLConverter);

// ---------------------------------------------------------------------------------------------
// HOCRConverter

/// Extract an hOCR representation from explicit text information within a PDF.
//   Where text is being extracted from a variety of types of PDF within a
//   business process, those PDFs where the text is only present in image
//   form will need to be analysed using an OCR tool which will typically
//   output hOCR. This converter extracts the explicit text information from
//   those PDFs that do have it and uses it to genxerate a basic hOCR
//   representation that is designed to be used in conjunction with the image
//   of the PDF in the same way as genuine OCR output would be, but without the
//   inevitable OCR errors.
//
//   The converter does not handle images, diagrams or text colors.
//
//   In the examples processed by the contributor it was necessary to set
//   LAParams.all_texts to True.
pub struct HOCRConverter {
    pub analyzer: PDFLayoutAnalyzer,
    pub conv: PDFConverter,
    pub stripcontrol: bool,
    pub within_chars: bool,
    pub page_bbox: Rect,
    pub working_text: PyStr,
    pub working_bbox: Rect,
    pub working_font: Value,
    pub working_size: f64,
}

impl HOCRConverter {
    /// `HOCRConverter(rsrcmgr, outfp, codec="utf8", pageno=1, laparams=None,
    /// stripcontrol=False)`.
    pub fn new(
        rsrcmgr: Rc<PDFResourceManager>,
        outfp: AnyIO,
        codec: &str,
        pageno: i64,
        laparams: Option<LAParams>,
        stripcontrol: bool,
    ) -> PyResult<Self> {
        let mut c = HOCRConverter {
            analyzer: PDFLayoutAnalyzer::new(rsrcmgr, pageno, laparams),
            conv: PDFConverter::new(outfp, codec),
            stripcontrol,
            within_chars: false,
            page_bbox: (0.0, 0.0, 0.0, 0.0),
            working_text: PyStr::new(),
            working_bbox: (0.0, 0.0, 0.0, 0.0),
            working_font: Value::None,
            working_size: 0.0,
        };
        c.write_header()?;
        Ok(c)
    }

    pub fn bbox_repr(&self, bbox: Rect) -> PyResult<String> {
        let (in_x0, in_y0, in_x1, in_y1) = bbox;
        // PDF y-coordinates are the other way round from hOCR coordinates
        let out_x0 = float_to_int(in_x0)?;
        let out_y0 = float_to_int(self.page_bbox.3 - in_y1)?;
        let out_x1 = float_to_int(in_x1)?;
        let out_y1 = float_to_int(self.page_bbox.3 - in_y0)?;
        Ok(format!("bbox {out_x0} {out_y0} {out_x1} {out_y1}"))
    }

    pub fn write(&self, text: &str) -> PyResult<()> {
        self.conv.write_codec_str(text)
    }

    pub fn write_header(&mut self) -> PyResult<()> {
        if !self.conv.codec.is_empty() {
            self.write(&format!(
                "<html xmlns='http://www.w3.org/1999/xhtml' xml:lang='en' lang='en' charset='{}'>\n",
                self.conv.codec
            ))?;
        } else {
            self.write("<html xmlns='http://www.w3.org/1999/xhtml' xml:lang='en' lang='en'>\n")?;
        }
        self.write("<head>\n")?;
        self.write("<title></title>\n")?;
        self.write("<meta http-equiv='Content-Type' content='text/html;charset=utf-8' />\n")?;
        self.write("<meta name='ocr-system' content='pdfminer.six HOCR Converter' />\n")?;
        self.write("  <meta name='ocr-capabilities' content='ocr_page ocr_block ocr_line ocrx_word'/>\n")?;
        self.write("</head>\n")?;
        self.write("<body>\n")
    }

    pub fn write_footer(&mut self) -> PyResult<()> {
        self.write("<!-- comment in the following line to debug -->\n")?;
        self.write("<!--script src='https://unpkg.com/hocrjs'></script--></body></html>\n")
    }

    pub fn write_text(&self, text: &PyStr) -> PyResult<()> {
        let text = if self.stripcontrol { strip_control(text) } else { text.clone() };
        self.conv.write_codec(&text)
    }

    pub fn write_word(&mut self) -> PyResult<()> {
        if !self.working_text.is_empty() {
            let mut bold_and_italic_styles = String::new();
            if py_contains_str(&self.working_font, "Italic")? {
                bold_and_italic_styles = "font-style: italic; ".into();
            }
            if py_contains_str(&self.working_font, "Bold")? {
                bold_and_italic_styles.push_str("font-weight: bold; ");
            }
            let font = self.working_font.py_str();
            let size = float_repr(self.working_size);
            let mut s = PyStr::from(format!(
                "<span style='font:\"{}\"; font-size:{}; {}' class='ocrx_word' title='{}; x_font {}; x_fsize {}'>",
                font,
                size,
                bold_and_italic_styles,
                self.bbox_repr(self.working_bbox)?,
                font,
                size
            ));
            s.push_pystr(&self.working_text.strip());
            s.push_str("</span>");
            self.conv.write_codec(&s)?;
        }
        self.within_chars = false;
        Ok(())
    }

    fn render(&mut self, item: &LTItem, depth: usize) -> PyResult<()> {
        enter_frame(depth)?;
        if self.within_chars && item.is_ltanno() {
            self.write_word()?;
        }
        if item.is_ltpage() {
            self.page_bbox = item.bbox();
            let pageid = item.as_page().expect("LTPage").pageid;
            self.write(&format!("<div class='ocr_page' id='{}' title='{}'>\n", pageid, self.bbox_repr(item.bbox())?))?;
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
            self.write("</div>\n")?;
        } else if item.is_lttextline() {
            self.write(&format!("<span class='ocr_line' title='{}'>", self.bbox_repr(item.bbox())?))?;
            for child_line in item.objs() {
                self.render(&child_line, depth + 1)?;
            }
            self.write("</span>\n")?;
        } else if item.is_lttextbox() {
            self.write(&format!(
                "<div class='ocr_block' id='{}' title='{}'>\n",
                item.index(),
                self.bbox_repr(item.bbox())?
            ))?;
            for child in item.objs() {
                self.render(&child, depth + 1)?;
            }
            self.write("</div>\n")?;
        } else if item.is_ltchar() {
            let c = item.as_char().expect("LTChar");
            let text = item.get_text();
            if !self.within_chars {
                self.within_chars = true;
                self.working_text = text;
                self.working_bbox = item.bbox();
                self.working_font = c.fontname.clone();
                self.working_size = c.size;
            } else if text.strip().is_empty() {
                self.write_word()?;
                self.conv.write_codec(&text)?;
            } else {
                #[allow(clippy::float_cmp)]
                if self.working_bbox.1 != item.bbox().1
                    || !py_eq(&self.working_font, &c.fontname)
                    || self.working_size != c.size
                {
                    self.write_word()?;
                    self.working_bbox = item.bbox();
                    self.working_font = c.fontname.clone();
                    self.working_size = c.size;
                }
                self.working_text.push_pystr(&text);
                self.working_bbox = (self.working_bbox.0, self.working_bbox.1, item.bbox().2, self.working_bbox.3);
            }
        }
        Ok(())
    }
}

impl LayoutAnalyzerDevice for HOCRConverter {
    fn analyzer(&self) -> &PDFLayoutAnalyzer {
        &self.analyzer
    }
    fn analyzer_mut(&mut self) -> &mut PDFLayoutAnalyzer {
        &mut self.analyzer
    }
    fn receive_layout(&mut self, ltpage: LTItem) -> PyResult<()> {
        self.render(&ltpage, ANALYZE_FRAME_DEPTH + 1)
    }
    fn close(&mut self) -> PyResult<()> {
        self.write_footer()
    }
}
impl_layout_device!(HOCRConverter);
