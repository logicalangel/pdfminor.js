//! Port of `pdfminer/psexceptions.py`: the classes are [`ExcKind`] variants
//! (`PSException`, `PSEOF`, `PSSyntaxError`, `PSTypeError`, `PSValueError`).

pub use crate::pyrt::error::ExcKind;
