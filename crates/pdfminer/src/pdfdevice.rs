//! Port of `pdfminer/pdfdevice.py`: the `PDFDevice` interface only so far.
//!
//! `PDFDevice` and its subclasses (`PDFTextDevice`, `TagExtractor`, the converters) become the
//! [`PDFDevice`] trait: one method per overridable Python method, with the base class's no-op as
//! the default. The interpreter holds its device as `Rc<RefCell<dyn PDFDevice>>`.

use std::rc::Rc;

use crate::pdfcolor::PDFColorSpace;
use crate::pdfinterp::{PDFGraphicStateRef, PDFTextState};
use crate::pdfpage::PDFPage;
use crate::pdftypes::PDFStream;
use crate::psparser::PSLiteral;
use crate::pyrt::error::PyResult;
use crate::pyrt::value::Value;
use crate::utils::{Matrix, PathSegment};

/// `PDFDevice`: translates the output of `PDFPageInterpreter` to the output that is needed.
pub trait PDFDevice {
    /// `self.ctm`.
    fn ctm(&self) -> Option<Matrix>;

    /// `set_ctm(ctm)`.
    fn set_ctm(&mut self, ctm: Matrix);

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

    /// `render_string(textstate, seq, ncs, graphicstate)`: `seq` is the `TJ` array (untyped
    /// items); text devices update `textstate.linematrix`.
    fn render_string(
        &mut self,
        _textstate: &mut PDFTextState,
        _seq: &[Value],
        _ncs: &PDFColorSpace,
        _graphicstate: &PDFGraphicStateRef,
    ) -> PyResult<()> {
        Ok(())
    }
}
