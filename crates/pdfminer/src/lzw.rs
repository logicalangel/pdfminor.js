//! Port of `pdfminer/lzw.py`: not yet ported (interface only).

use crate::pyrt::error::PyResult;

/// `lzwdecode(data)`.
pub fn lzwdecode(_data: &[u8]) -> PyResult<Vec<u8>> {
    todo!("port pdfminer/lzw.py")
}
