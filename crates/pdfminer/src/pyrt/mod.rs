//! The Python runtime semantics the port depends on.
//!
//! pdfminer's results depend on Python's rules, not just on its algorithms: number formatting,
//! `int()`/`float()` of odd objects, exception classes, `str.isspace()`, lone surrogates in `str`,
//! the exact order `heapq` pops ties. Each module ports one such rule, and the ported pdfminer
//! modules use them instead of the Rust equivalents (see `docs/PORTING.md`).

pub mod ctype;
pub mod error;
pub mod fmt;
pub mod heapq;
pub mod io;
pub mod pystr;
pub mod sort;
pub mod unicode_tables;
pub mod value;

pub use error::{ExcKind, PyErr, PyResult, catch};
pub use pystr::PyStr;
pub use value::{Dict, DictRef, ListRef, Value};
mod timsort;
pub mod codecs;
pub mod zlib;
pub mod num;
