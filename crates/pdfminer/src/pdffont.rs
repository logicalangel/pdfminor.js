//! Port of `pdfminer/pdffont.py`: not yet ported (interface only).
//!
//! The public methods below are the contract the rest of the port (pdfdevice, layout,
//! pdfinterp) codes against; the port fills them in and adds the rest of the module.

use crate::pyrt::error::PyResult;
use crate::pyrt::pystr::PyStr;
use crate::pyrt::value::Value;

/// A font (`PDFFont` and its subclasses `PDFSimpleFont`, `PDFType1Font`, `PDFTrueTypeFont`,
/// `PDFType3Font`, `PDFCIDFont`). Shared as `Rc<PDFFont>`.
pub struct PDFFont {}

impl PDFFont {
    /// `font.fontname` (usually a `str`, but Python keeps whatever the descriptor holds).
    pub fn fontname(&self) -> &Value {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.is_vertical()`.
    pub fn is_vertical(&self) -> bool {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.is_multibyte()`.
    pub fn is_multibyte(&self) -> bool {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.decode(bytes)`: the character ids.
    pub fn decode(&self, _bytes: &[u8]) -> PyResult<Vec<i64>> {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.get_ascent()`.
    pub fn get_ascent(&self) -> PyResult<f64> {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.get_descent()`.
    pub fn get_descent(&self) -> PyResult<f64> {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.get_width()`.
    pub fn get_width(&self) -> PyResult<f64> {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.get_height()`.
    pub fn get_height(&self) -> PyResult<f64> {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.char_width(cid)`.
    pub fn char_width(&self, _cid: i64) -> PyResult<f64> {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.char_disp(cid)`: a number for horizontal fonts (the int `0` by default), a tuple
    /// `(vx or None, vy)` for vertical fonts. Kept as the Python object.
    pub fn char_disp(&self, _cid: i64) -> PyResult<Value> {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.string_width(s)`.
    pub fn string_width(&self, _s: &[u8]) -> PyResult<f64> {
        todo!("port pdfminer/pdffont.py")
    }

    /// `font.to_unichr(cid)`: raises `PDFUnicodeNotDefined` (or `NotImplementedError` for the
    /// base class) when the character has no Unicode mapping.
    pub fn to_unichr(&self, _cid: i64) -> PyResult<PyStr> {
        todo!("port pdfminer/pdffont.py")
    }

    /// `repr(font)`.
    pub fn repr(&self) -> String {
        todo!("port pdfminer/pdffont.py")
    }
}
