//! Port of `pdfminer/runlength.py`: not yet ported (interface only).

use crate::pyrt::error::PyResult;

/// `rldecode(data)`.
pub fn rldecode(_data: &[u8]) -> PyResult<Vec<u8>> {
    todo!("port pdfminer/runlength.py")
}
