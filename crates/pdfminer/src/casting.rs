//! Port of `pdfminer/casting.py`.

use crate::pyrt::error::{ExcKind, PyResult};
use crate::pyrt::value::{Value, py_float, py_int};
use crate::utils::{Matrix, Rect};

/// `safe_int(o)`: `int(o)`, or `None` on `TypeError`/`ValueError` (an `OverflowError`, from
/// `int(inf)`, propagates as in Python).
pub fn safe_int(o: &Value) -> PyResult<Option<i64>> {
    match py_int(o) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.is_any(&[ExcKind::TypeError, ExcKind::ValueError]) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `safe_float(o)`: `float(o)`, or `None` on `TypeError`/`ValueError`/`OverflowError`.
pub fn safe_float(o: &Value) -> Option<f64> {
    py_float(o).ok()
}

/// `safe_matrix(a, b, c, d, e, f)`.
pub fn safe_matrix(a: &Value, b: &Value, c: &Value, d: &Value, e: &Value, f: &Value) -> Option<Matrix> {
    Some((safe_float(a)?, safe_float(b)?, safe_float(c)?, safe_float(d)?, safe_float(e)?, safe_float(f)?))
}

/// `safe_rgb(r, g, b)`.
pub fn safe_rgb(r: &Value, g: &Value, b: &Value) -> Option<(f64, f64, f64)> {
    Some((safe_float(r)?, safe_float(g)?, safe_float(b)?))
}

/// `safe_cmyk(c, m, y, k)`.
pub fn safe_cmyk(c: &Value, m: &Value, y: &Value, k: &Value) -> Option<(f64, f64, f64, f64)> {
    Some((safe_float(c)?, safe_float(m)?, safe_float(y)?, safe_float(k)?))
}

/// `list(itertools.islice(value, n))`: the first `n` items of any iterable; `Ok(None)` when
/// `iter(value)` raises `TypeError`.
pub fn py_islice(value: &Value, n: usize) -> PyResult<Option<Vec<Value>>> {
    Ok(Some(match value {
        Value::List(l) => l.borrow().iter().take(n).cloned().collect(),
        Value::Tuple(t) => t.iter().take(n).cloned().collect(),
        Value::Bytes(b) => b.iter().take(n).map(|&c| Value::Int(c as i64)).collect(),
        Value::Str(s) => s.chars().take(n).map(|c| Value::str(&c.to_string())).collect(),
        Value::Dict(d) => d.borrow().keys().take(n).map(|k| Value::Str(k.clone())).collect(),
        // A PDFStream has __getitem__ but no __iter__: Python iterates stream[0], stream[1], ...
        // and stream[0] raises PDFKeyError (not IndexError), which propagates.
        Value::Stream(s) => {
            if n == 0 {
                return Ok(Some(vec![]));
            }
            return Err(crate::pyrt::error::PyErr::new(
                ExcKind::PDFKeyError,
                crate::pyrt::fmt::str_repr(&format!(
                    "PDF stream object {} does not have attribute '0'",
                    s.objid.get().map_or("None".to_string(), |o| o.to_string())
                )),
            ));
        }
        _ => return Ok(None),
    }))
}

/// `safe_rect_list(value)`.
pub fn safe_rect_list(value: &Value) -> PyResult<Option<Rect>> {
    let Some(values) = py_islice(value, 4)? else {
        return Ok(None);
    };
    if values.len() != 4 {
        return Ok(None);
    }
    Ok(safe_rect(&values[0], &values[1], &values[2], &values[3]))
}

/// `safe_rect(a, b, c, d)`.
pub fn safe_rect(a: &Value, b: &Value, c: &Value, d: &Value) -> Option<Rect> {
    Some((safe_float(a)?, safe_float(b)?, safe_float(c)?, safe_float(d)?))
}
