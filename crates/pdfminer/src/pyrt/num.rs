//! Float arithmetic that raises like Python.
//!
//! Python's `/`, `//` and `%` raise `ZeroDivisionError` for a zero divisor even on floats (no
//! inf/NaN), and `**` raises `OverflowError` where C returns inf. Ported arithmetic on `f64` uses
//! these wherever the divisor can be zero.

use super::error::{ExcKind, PyErr, PyResult};
use super::value::{float_floordiv, float_mod};

/// `a / b` for floats.
#[inline]
pub fn div(a: f64, b: f64) -> PyResult<f64> {
    if b == 0.0 {
        return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
    }
    Ok(a / b)
}

/// `a // b` for floats.
#[inline]
pub fn floordiv(a: f64, b: f64) -> PyResult<f64> {
    if b == 0.0 {
        return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
    }
    Ok(float_floordiv(a, b))
}

/// `a % b` for floats.
#[inline]
pub fn rem(a: f64, b: f64) -> PyResult<f64> {
    if b == 0.0 {
        return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
    }
    Ok(float_mod(a, b))
}

/// `a ** b` for floats (CPython's `float_pow`: C `pow`, with Python's errors).
pub fn pow(a: f64, b: f64) -> PyResult<f64> {
    if a == 0.0 && b < 0.0 {
        return Err(PyErr::new(ExcKind::ZeroDivisionError, "zero to a negative power"));
    }
    if a < 0.0 && b != b.floor() && b.is_finite() {
        // Python returns a complex number here; pdfminer never relies on it.
        return Err(PyErr::new(ExcKind::ValueError, "negative number cannot be raised to a fractional power"));
    }
    let r = a.powf(b);
    if r.is_infinite() && a.is_finite() && b.is_finite() {
        return Err(PyErr::new(ExcKind::OverflowError, "(34, 'Numerical result out of range')"));
    }
    Ok(r)
}
