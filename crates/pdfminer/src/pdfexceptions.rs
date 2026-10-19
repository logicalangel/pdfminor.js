//! Port of `pdfminer/pdfexceptions.py`: the classes are [`ExcKind`] variants with the same
//! (multiple) inheritance, e.g. `PDFKeyError` is both a `PDFException` and a `KeyError`.

pub use crate::pyrt::error::ExcKind;
