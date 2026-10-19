//! Port of `pdfminer/ascii85.py`: not yet ported (interface only).

use crate::pyrt::error::PyResult;

/// `ascii85decode(data)`.
pub fn ascii85decode(_data: &[u8]) -> PyResult<Vec<u8>> {
    todo!("port pdfminer/ascii85.py")
}

/// `asciihexdecode(data)`.
pub fn asciihexdecode(_data: &[u8]) -> PyResult<Vec<u8>> {
    todo!("port pdfminer/ascii85.py")
}
