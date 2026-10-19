//! Port of `pdfminer/ccitt.py`: not yet ported (interface only).

use crate::pyrt::error::PyResult;
use crate::pyrt::value::Value;

/// `ccittfaxdecode(data, params)`.
pub fn ccittfaxdecode(_data: &[u8], _params: &Value) -> PyResult<Vec<u8>> {
    todo!("port pdfminer/ccitt.py")
}
