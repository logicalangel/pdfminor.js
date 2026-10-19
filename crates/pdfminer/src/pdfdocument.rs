//! Port of `pdfminer/pdfdocument.py`: not yet ported (interface only).

use std::rc::Rc;

use crate::pdftypes::DecipherCallable;
use crate::pyrt::error::PyResult;
use crate::pyrt::value::Value;

/// `PDFDocument`. Created as `Rc<PDFDocument>` (its parser and every `PDFObjRef` hold a `Weak`
/// back-reference); caches and parser state use interior mutability.
pub struct PDFDocument {}

impl PDFDocument {
    /// `doc.getobj(objid)`: `PDFObjectNotFound` when missing.
    pub fn getobj(&self, _objid: i64) -> PyResult<Value> {
        todo!("port pdfminer/pdfdocument.py")
    }

    /// `doc.decipher`: the security handler's `decrypt`, or `None` for unencrypted documents.
    pub fn decipher(&self) -> Option<Rc<DecipherCallable>> {
        todo!("port pdfminer/pdfdocument.py")
    }
}
