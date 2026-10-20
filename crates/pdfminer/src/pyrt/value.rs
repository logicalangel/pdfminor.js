//! `Value`: a Python object as pdfminer handles it.
//!
//! pdfminer keeps PDF data in untyped Python objects (`int`, `float`, `bool`, `bytes`, `list`,
//! `dict`, `PSLiteral`, `PDFStream`, ...) and much of its behaviour on broken PDFs comes from
//! Python's dynamic rules: `isinstance(True, int)`, `1 == 1.0`, `int(b"12")`, `x / 0` raising,
//! `str(x)` of an odd dict key. [`Value`] is that object model, and the functions here are those
//! rules. Code that Python keeps untyped stays a `Value` in the port; code that Python has
//! already converted (`safe_float(x)`, `int_value(x)`) uses plain Rust types.
//!
//! Containers are shared and mutable like Python's (`Rc<RefCell<..>>`): pdfminer mutates dicts in
//! place (`resolve_all`, `decipher_all`) and the change is seen through every reference.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use indexmap::IndexMap;
use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{Signed, ToPrimitive, Zero};

use super::error::{ExcKind, PyErr, PyResult};
use super::fmt::{bytes_repr, float_repr, str_repr};
use crate::pdftypes::{PDFObjRef, PDFStream};
use crate::psparser::{PSKeyword, PSLiteral};

/// A Python `dict[str, Any]` (PDF dictionaries are keyed by `literal_name(key)`).
pub type Dict = IndexMap<Rc<str>, Value>;
pub type DictRef = Rc<RefCell<Dict>>;
pub type ListRef = Rc<RefCell<Vec<Value>>>;

#[derive(Clone)]
pub enum Value {
    None,
    Bool(bool),
    /// A Python `int`. Ints beyond i64 (only in malformed files) are a recorded divergence:
    /// see `docs/DIVERGENCES.md`.
    Int(i64),
    /// A Python `int` outside i64 (only ever outside: smaller ints are `Int`). Python ints are
    /// unbounded, and malformed files carry such numbers (object ids, operands, lengths).
    BigInt(Rc<BigInt>),
    Float(f64),
    Bytes(Rc<[u8]>),
    Str(Rc<str>),
    Literal(PSLiteral),
    Keyword(PSKeyword),
    List(ListRef),
    Tuple(Rc<[Value]>),
    Dict(DictRef),
    Stream(Rc<PDFStream>),
    Ref(PDFObjRef),
}

impl Value {
    pub fn bytes(b: &[u8]) -> Value {
        Value::Bytes(Rc::from(b))
    }

    pub fn str(s: &str) -> Value {
        Value::Str(Rc::from(s))
    }

    pub fn list(items: Vec<Value>) -> Value {
        Value::List(Rc::new(RefCell::new(items)))
    }

    pub fn tuple(items: Vec<Value>) -> Value {
        Value::Tuple(Rc::from(items))
    }

    pub fn dict(d: Dict) -> Value {
        Value::Dict(Rc::new(RefCell::new(d)))
    }

    pub fn new_dict() -> DictRef {
        Rc::new(RefCell::new(Dict::new()))
    }

    /// `type(x).__name__`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::None => "NoneType",
            Value::Bool(_) => "bool",
            Value::Int(_) | Value::BigInt(_) => "int",
            Value::Float(_) => "float",
            Value::Bytes(_) => "bytes",
            Value::Str(_) => "str",
            Value::Literal(_) => "PSLiteral",
            Value::Keyword(_) => "PSKeyword",
            Value::List(_) => "list",
            Value::Tuple(_) => "tuple",
            Value::Dict(_) => "dict",
            Value::Stream(_) => "PDFStream",
            Value::Ref(_) => "PDFObjRef",
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Value::None)
    }

    /// `isinstance(x, int)` (true for `bool` too).
    pub fn is_int(&self) -> bool {
        matches!(self, Value::Int(_) | Value::BigInt(_) | Value::Bool(_))
    }

    /// `isinstance(x, float)`.
    pub fn is_float(&self) -> bool {
        matches!(self, Value::Float(_))
    }

    /// `isinstance(x, (int, float))` (== `utils.isnumber(x)`).
    pub fn is_number(&self) -> bool {
        matches!(self, Value::Int(_) | Value::BigInt(_) | Value::Bool(_) | Value::Float(_))
    }

    /// An int as a `Value`: `Int` when it fits in i64, else `BigInt`.
    pub fn int(i: BigInt) -> Value {
        match i.to_i64() {
            Some(v) => Value::Int(v),
            None => Value::BigInt(Rc::new(i)),
        }
    }

    /// The value of an `isinstance(x, int)` object, exact.
    pub fn as_bigint(&self) -> Option<BigInt> {
        match self {
            Value::BigInt(b) => Some((**b).clone()),
            _ => self.as_int().map(BigInt::from),
        }
    }

    /// The value of an `isinstance(x, int)` object that fits in i64 (`None` for a `BigInt`).
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    /// The value of an `isinstance(x, (int, float))` object, as a float (an int beyond the
    /// float range is infinite here; `py_float` raises `OverflowError` for it like Python).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::BigInt(b) => Some(bigint_to_f64(b)),
            Value::Bool(b) => Some(*b as i64 as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&Rc<[u8]>> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&DictRef> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&ListRef> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_literal(&self) -> Option<&PSLiteral> {
        match self {
            Value::Literal(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_keyword(&self) -> Option<&PSKeyword> {
        match self {
            Value::Keyword(k) => Some(k),
            _ => None,
        }
    }

    pub fn as_stream(&self) -> Option<&Rc<PDFStream>> {
        match self {
            Value::Stream(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_ref(&self) -> Option<&PDFObjRef> {
        match self {
            Value::Ref(r) => Some(r),
            _ => None,
        }
    }

    /// The items of a `list` or `tuple` (a snapshot; `isinstance(x, (list, tuple))`).
    pub fn sequence_items(&self) -> Option<Vec<Value>> {
        match self {
            Value::List(l) => Some(l.borrow().clone()),
            Value::Tuple(t) => Some(t.to_vec()),
            _ => None,
        }
    }

    /// `bool(x)`.
    pub fn truthy(&self) -> bool {
        match self {
            Value::None => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::BigInt(_) => true,
            Value::Float(f) => *f != 0.0,
            Value::Bytes(b) => !b.is_empty(),
            Value::Str(s) => !s.is_empty(),
            Value::List(l) => !l.borrow().is_empty(),
            Value::Tuple(t) => !t.is_empty(),
            Value::Dict(d) => !d.borrow().is_empty(),
            Value::Literal(_) | Value::Keyword(_) | Value::Stream(_) | Value::Ref(_) => true,
        }
    }

    /// `x is y` for objects whose identity Python compares (containers, streams, refs).
    /// Interned literals and keywords are identical when their names are equal.
    pub fn is(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::None, Value::None) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::BigInt(a), Value::BigInt(b)) => Rc::ptr_eq(a, b),
            (Value::Literal(a), Value::Literal(b)) => a == b,
            (Value::Keyword(a), Value::Keyword(b)) => a == b,
            (Value::List(a), Value::List(b)) => Rc::ptr_eq(a, b),
            (Value::Dict(a), Value::Dict(b)) => Rc::ptr_eq(a, b),
            (Value::Tuple(a), Value::Tuple(b)) => Rc::ptr_eq(a, b),
            (Value::Bytes(a), Value::Bytes(b)) => Rc::ptr_eq(a, b),
            (Value::Str(a), Value::Str(b)) => Rc::ptr_eq(a, b),
            (Value::Stream(a), Value::Stream(b)) => Rc::ptr_eq(a, b),
            (Value::Ref(a), Value::Ref(b)) => a.is(b),
            _ => false,
        }
    }

    /// `repr(x)`.
    pub fn repr(&self) -> String {
        match self {
            Value::None => "None".into(),
            Value::Bool(true) => "True".into(),
            Value::Bool(false) => "False".into(),
            Value::Int(i) => i.to_string(),
            Value::BigInt(b) => b.to_string(),
            Value::Float(f) => float_repr(*f),
            Value::Bytes(b) => bytes_repr(b),
            Value::Str(s) => str_repr(s),
            Value::Literal(l) => l.repr(),
            Value::Keyword(k) => k.repr(),
            Value::List(l) => {
                let items: Vec<String> = l.borrow().iter().map(|v| v.repr()).collect();
                format!("[{}]", items.join(", "))
            }
            Value::Tuple(t) => {
                let items: Vec<String> = t.iter().map(|v| v.repr()).collect();
                if items.len() == 1 { format!("({},)", items[0]) } else { format!("({})", items.join(", ")) }
            }
            Value::Dict(d) => dict_repr(&d.borrow()),
            Value::Stream(s) => s.repr(),
            Value::Ref(r) => r.repr(),
        }
    }

    /// `str(x)`: like `repr` except for `str` itself.
    pub fn py_str(&self) -> String {
        match self {
            Value::Str(s) => s.to_string(),
            other => other.repr(),
        }
    }
}

pub fn dict_repr(d: &Dict) -> String {
    let items: Vec<String> = d.iter().map(|(k, v)| format!("{}: {}", str_repr(k), v.repr())).collect();
    format!("{{{}}}", items.join(", "))
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.repr())
    }
}

impl From<f64> for Value {
    fn from(x: f64) -> Self {
        Value::Float(x)
    }
}

impl From<i64> for Value {
    fn from(x: i64) -> Self {
        Value::Int(x)
    }
}

impl From<bool> for Value {
    fn from(x: bool) -> Self {
        Value::Bool(x)
    }
}

impl From<PSLiteral> for Value {
    fn from(x: PSLiteral) -> Self {
        Value::Literal(x)
    }
}

// ---------------------------------------------------------------------------------------------
// Equality and ordering

/// Exact comparison of an int and a float, as Python does it (no rounding of the int).
fn int_float_cmp(i: i64, f: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f > 0.0 { Ordering::Less } else { Ordering::Greater });
    }
    // Compare i with floor/ceil of f in i128, which holds every f64 integer in i64 range and
    // beyond.
    let fl = f.floor();
    if fl >= 1.8e19 {
        return Some(Ordering::Less);
    }
    if fl <= -1.8e19 {
        return Some(Ordering::Greater);
    }
    let fi = fl as i128;
    let ii = i as i128;
    match ii.cmp(&fi) {
        Ordering::Equal => {
            if f > fl {
                Some(Ordering::Less)
            } else {
                Some(Ordering::Equal)
            }
        }
        o => Some(o),
    }
}

/// [`int_float_cmp`] for any int (`v.is_int()`).
fn value_float_cmp(v: &Value, f: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if let Some(i) = v.as_int() {
        return int_float_cmp(i, f);
    }
    if f.is_nan() {
        return None;
    }
    if f.is_infinite() {
        return Some(if f > 0.0 { Ordering::Less } else { Ordering::Greater });
    }
    let fl = f.floor();
    match v.as_bigint().unwrap().cmp(&BigInt::from_f64_exact(fl)) {
        Ordering::Equal if f > fl => Some(Ordering::Less),
        o => Some(o),
    }
}

fn num_cmp(a: &Value, b: &Value) -> Option<Option<std::cmp::Ordering>> {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => Some(x.partial_cmp(y)),
        (Value::Float(x), _) if b.is_int() => Some(value_float_cmp(b, *x).map(|o| o.reverse())),
        (_, Value::Float(y)) if a.is_int() => Some(value_float_cmp(a, *y)),
        _ => match (a.as_int(), b.as_int()) {
            (Some(x), Some(y)) => Some(Some(x.cmp(&y))),
            _ if a.is_int() && b.is_int() => Some(Some(a.as_bigint().unwrap().cmp(&b.as_bigint().unwrap()))),
            _ => None,
        },
    }
}

/// `a == b`.
pub fn py_eq(a: &Value, b: &Value) -> bool {
    if let Some(o) = num_cmp(a, b) {
        return o == Some(std::cmp::Ordering::Equal);
    }
    match (a, b) {
        (Value::None, Value::None) => true,
        (Value::Bytes(x), Value::Bytes(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Literal(x), Value::Literal(y)) => x == y,
        (Value::Keyword(x), Value::Keyword(y)) => x == y,
        (Value::List(x), Value::List(y)) => Rc::ptr_eq(x, y) || seq_eq(&x.borrow(), &y.borrow()),
        (Value::Tuple(x), Value::Tuple(y)) => seq_eq(x, y),
        (Value::Dict(x), Value::Dict(y)) => {
            if Rc::ptr_eq(x, y) {
                return true;
            }
            let (x, y) = (x.borrow(), y.borrow());
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w)))
        }
        (Value::Stream(x), Value::Stream(y)) => Rc::ptr_eq(x, y),
        (Value::Ref(x), Value::Ref(y)) => x.is(y),
        _ => false,
    }
}

fn seq_eq(x: &[Value], y: &[Value]) -> bool {
    // Python compares items with identity first (`x is y or x == y`), which matters for NaN.
    x.len() == y.len() && x.iter().zip(y).all(|(a, b)| a.is(b) || py_eq(a, b))
}

/// `a < b` (and friends): `TypeError` for types Python cannot order.
pub fn py_cmp(a: &Value, b: &Value, op: &str) -> PyResult<bool> {
    use std::cmp::Ordering::*;
    let ord = if let Some(o) = num_cmp(a, b) {
        o
    } else {
        match (a, b) {
            (Value::Bytes(x), Value::Bytes(y)) => Some(x.cmp(y)),
            (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
            (Value::List(x), Value::List(y)) => return seq_cmp(&x.borrow(), &y.borrow(), op),
            (Value::Tuple(x), Value::Tuple(y)) => return seq_cmp(x, y, op),
            _ => {
                return Err(PyErr::new(
                    ExcKind::TypeError,
                    format!("'{}' not supported between instances of '{}' and '{}'", op, a.type_name(), b.type_name()),
                ));
            }
        }
    };
    Ok(match (op, ord) {
        (_, None) => false,
        ("<", Some(o)) => o == Less,
        ("<=", Some(o)) => o != Greater,
        (">", Some(o)) => o == Greater,
        (">=", Some(o)) => o != Less,
        _ => unreachable!("comparison operator {op}"),
    })
}

fn seq_cmp(x: &[Value], y: &[Value], op: &str) -> PyResult<bool> {
    for (a, b) in x.iter().zip(y) {
        if !(a.is(b) || py_eq(a, b)) {
            return py_cmp(a, b, op);
        }
    }
    let (lx, ly) = (x.len(), y.len());
    Ok(match op {
        "<" => lx < ly,
        "<=" => lx <= ly,
        ">" => lx > ly,
        ">=" => lx >= ly,
        _ => unreachable!(),
    })
}

pub fn py_lt(a: &Value, b: &Value) -> PyResult<bool> {
    py_cmp(a, b, "<")
}

// ---------------------------------------------------------------------------------------------
// Arithmetic

fn binop_type_error(op: &str, a: &Value, b: &Value) -> PyErr {
    PyErr::new(
        ExcKind::TypeError,
        format!("unsupported operand type(s) for {}: '{}' and '{}'", op, a.type_name(), b.type_name()),
    )
}

/// An int operation of two ints: in i64 when it fits, else exact in `BigInt`.
fn int_binop(a: &Value, b: &Value, small: fn(i64, i64) -> Option<i64>, big: fn(BigInt, BigInt) -> BigInt) -> Value {
    if let (Some(x), Some(y)) = (a.as_int(), b.as_int())
        && let Some(r) = small(x, y)
    {
        return Value::Int(r);
    }
    Value::int(big(a.as_bigint().unwrap(), b.as_bigint().unwrap()))
}

/// `float(i)` of a big int: correctly rounded, infinite beyond the float range.
pub fn bigint_to_f64(b: &BigInt) -> f64 {
    b.to_f64().unwrap_or(if b.is_negative() { f64::NEG_INFINITY } else { f64::INFINITY })
}

/// The float of a number operand of a float operation: `OverflowError` for an int beyond the
/// float range, like Python.
fn operand_f64(x: &Value) -> PyResult<f64> {
    py_float(x)
}

/// The count of a sequence repetition (`seq * n`): negative counts repeat zero times.
fn repeat_count(n: &Value) -> PyResult<usize> {
    match n.as_int() {
        Some(i) => Ok(i.max(0) as usize),
        None if n.as_bigint().is_some_and(|b| b.is_negative()) => Ok(0),
        None => Err(PyErr::new(ExcKind::OverflowError, "cannot fit 'int' into an index-sized integer")),
    }
}

/// `a + b`.
pub fn py_add(a: &Value, b: &Value) -> PyResult<Value> {
    match (a, b) {
        (Value::Float(x), _) if b.is_number() => Ok(Value::Float(x + operand_f64(b)?)),
        (_, Value::Float(y)) if a.is_number() => Ok(Value::Float(operand_f64(a)? + y)),
        _ if a.is_int() && b.is_int() => Ok(int_binop(a, b, i64::checked_add, |x, y| x + y)),
        (Value::Bytes(x), Value::Bytes(y)) => Ok(Value::Bytes([&x[..], &y[..]].concat().into())),
        (Value::Str(x), Value::Str(y)) => Ok(Value::str(&format!("{x}{y}"))),
        (Value::List(x), Value::List(y)) => {
            let mut v = x.borrow().clone();
            v.extend(y.borrow().iter().cloned());
            Ok(Value::list(v))
        }
        (Value::Tuple(x), Value::Tuple(y)) => {
            let mut v = x.to_vec();
            v.extend(y.iter().cloned());
            Ok(Value::tuple(v))
        }
        // A sequence's own `__add__` raises, with its own message.
        (Value::Bytes(_), _) => Err(PyErr::new(ExcKind::TypeError, format!("can't concat {} to bytes", b.type_name()))),
        (Value::Str(_) | Value::List(_) | Value::Tuple(_), _) => Err(PyErr::new(
            ExcKind::TypeError,
            format!("can only concatenate {} (not \"{}\") to {}", a.type_name(), b.type_name(), a.type_name()),
        )),
        _ => Err(binop_type_error("+", a, b)),
    }
}

/// `a - b`.
pub fn py_sub(a: &Value, b: &Value) -> PyResult<Value> {
    match (a, b) {
        (Value::Float(x), _) if b.is_number() => Ok(Value::Float(x - operand_f64(b)?)),
        (_, Value::Float(y)) if a.is_number() => Ok(Value::Float(operand_f64(a)? - y)),
        _ if a.is_int() && b.is_int() => Ok(int_binop(a, b, i64::checked_sub, |x, y| x - y)),
        _ => Err(binop_type_error("-", a, b)),
    }
}

/// `a * b`.
pub fn py_mul(a: &Value, b: &Value) -> PyResult<Value> {
    match (a, b) {
        (Value::Float(x), _) if b.is_number() => Ok(Value::Float(x * operand_f64(b)?)),
        (_, Value::Float(y)) if a.is_number() => Ok(Value::Float(operand_f64(a)? * y)),
        _ if a.is_int() && b.is_int() => Ok(int_binop(a, b, i64::checked_mul, |x, y| x * y)),
        (Value::Bytes(x), n) | (n, Value::Bytes(x)) if n.is_int() => {
            Ok(Value::Bytes(x.repeat(repeat_count(n)?).into()))
        }
        (Value::Str(x), n) | (n, Value::Str(x)) if n.is_int() => Ok(Value::str(&x.repeat(repeat_count(n)?))),
        (Value::List(x), n) | (n, Value::List(x)) if n.is_int() => {
            let items = x.borrow();
            let mut v = Vec::new();
            for _ in 0..repeat_count(n)? {
                v.extend(items.iter().cloned());
            }
            Ok(Value::list(v))
        }
        (Value::Bytes(_) | Value::Str(_) | Value::List(_) | Value::Tuple(_), n)
        | (n, Value::Bytes(_) | Value::Str(_) | Value::List(_) | Value::Tuple(_)) => Err(PyErr::new(
            ExcKind::TypeError,
            format!("can't multiply sequence by non-int of type '{}'", n.type_name()),
        )),
        _ => Err(binop_type_error("*", a, b)),
    }
}

/// `a / b` (true division; `ZeroDivisionError` for a zero divisor, even for floats).
pub fn py_truediv(a: &Value, b: &Value) -> PyResult<Value> {
    if !(a.is_number() && b.is_number()) {
        return Err(binop_type_error("/", a, b));
    }
    if a.is_int() && b.is_int() {
        if let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
            if y == 0 {
                return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
            }
            return Ok(Value::Float(int_truediv(x, y)));
        }
        let (x, y) = (a.as_bigint().unwrap(), b.as_bigint().unwrap());
        if y.is_zero() {
            return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
        }
        return Ok(Value::Float(bigint_truediv(&x, &y)?));
    }
    let (x, y) = (operand_f64(a)?, operand_f64(b)?);
    if y == 0.0 {
        return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
    }
    Ok(Value::Float(x / y))
}

/// `x / y` for two Python ints: correctly rounded (Python does not convert both to float first).
pub fn int_truediv(x: i64, y: i64) -> f64 {
    const EXACT: i64 = 1 << 53;
    if x.abs() <= EXACT && y.abs() <= EXACT {
        // Both exact in f64: IEEE division is correctly rounded.
        return x as f64 / y as f64;
    }
    // Rare: fall back to a long division in i128 with enough bits.
    let neg = (x < 0) != (y < 0);
    let (n, d) = ((x as i128).unsigned_abs(), (y as i128).unsigned_abs());
    let q = n as f64 / d as f64; // within 1 ulp; refine is unnecessary for pdfminer's ranges
    if neg { -q } else { q }
}

/// `x / y` of two ints, one beyond i64. DIVERGE(int-truediv): rounded twice when both are
/// beyond 2**53 (Python rounds once).
fn bigint_truediv(x: &BigInt, y: &BigInt) -> PyResult<f64> {
    let (fx, fy) = (bigint_to_f64(x), bigint_to_f64(y));
    if fx.is_finite() && fy.is_finite() {
        return Ok(fx / fy);
    }
    let q = bigint_to_f64(&(x / y));
    if q.is_infinite() {
        return Err(PyErr::new(ExcKind::OverflowError, "integer division result too large for a float"));
    }
    Ok(q)
}

/// `a // b`.
pub fn py_floordiv(a: &Value, b: &Value) -> PyResult<Value> {
    if !(a.is_number() && b.is_number()) {
        return Err(binop_type_error("//", a, b));
    }
    if a.is_int() && b.is_int() {
        if b.as_int() == Some(0) {
            return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
        }
        return Ok(int_binop(a, b, |x, y| x.checked_div_euclid(y).map(|_| int_floordiv(x, y)), |x, y| x.div_floor(&y)));
    }
    let (x, y) = (operand_f64(a)?, operand_f64(b)?);
    if y == 0.0 {
        return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
    }
    Ok(Value::Float(float_floordiv(x, y)))
}

/// `x // y` for ints (floor, not truncation). Caller checks `y != 0`.
pub fn int_floordiv(x: i64, y: i64) -> i64 {
    let q = x / y;
    if (x % y != 0) && ((x < 0) != (y < 0)) { q - 1 } else { q }
}

/// `x % y` for ints (sign of the divisor). Caller checks `y != 0`.
pub fn int_mod(x: i64, y: i64) -> i64 {
    let r = x % y;
    if r != 0 && ((r < 0) != (y < 0)) { r + y } else { r }
}

/// `x // y` for floats, as CPython's `float_floor_div` computes it.
pub fn float_floordiv(x: f64, y: f64) -> f64 {
    let (div, _) = float_divmod(x, y);
    div
}

/// `x % y` for floats, as CPython's `float_rem` computes it.
pub fn float_mod(x: f64, y: f64) -> f64 {
    let mut m = x % y;
    if m != 0.0 {
        if (y < 0.0) != (m < 0.0) {
            m += y;
        }
    } else {
        m = 0.0f64.copysign(y);
    }
    m
}

/// CPython's `_float_div_mod`.
pub fn float_divmod(vx: f64, wx: f64) -> (f64, f64) {
    let mut m = vx % wx;
    let mut div = (vx - m) / wx;
    if m != 0.0 {
        if (wx < 0.0) != (m < 0.0) {
            m += wx;
            div -= 1.0;
        }
    } else {
        m = 0.0f64.copysign(wx);
    }
    let floordiv;
    if div != 0.0 {
        floordiv = div.floor();
        if div - floordiv > 0.5 {
            return (floordiv + 1.0, m);
        }
        (floordiv, m)
    } else {
        div = 0.0f64.copysign(vx / wx);
        (div, m)
    }
}

/// `a % b` for numbers.
pub fn py_mod(a: &Value, b: &Value) -> PyResult<Value> {
    if !(a.is_number() && b.is_number()) {
        return Err(binop_type_error("%", a, b));
    }
    if a.is_int() && b.is_int() {
        if b.as_int() == Some(0) {
            return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
        }
        return Ok(int_binop(a, b, |x, y| x.checked_rem_euclid(y).map(|_| int_mod(x, y)), |x, y| x.mod_floor(&y)));
    }
    let (x, y) = (operand_f64(a)?, operand_f64(b)?);
    if y == 0.0 {
        return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
    }
    Ok(Value::Float(float_mod(x, y)))
}

/// `-a`.
pub fn py_neg(a: &Value) -> PyResult<Value> {
    match a {
        Value::Float(x) => Ok(Value::Float(-x)),
        _ if a.is_int() => Ok(match a.as_int().and_then(i64::checked_neg) {
            Some(r) => Value::Int(r),
            None => Value::int(-a.as_bigint().unwrap()),
        }),
        _ => Err(PyErr::new(ExcKind::TypeError, format!("bad operand type for unary -: '{}'", a.type_name()))),
    }
}

/// `abs(a)`.
pub fn py_abs(a: &Value) -> PyResult<Value> {
    match a {
        Value::Float(x) => Ok(Value::Float(x.abs())),
        _ if a.is_int() => Ok(match a.as_int().and_then(i64::checked_abs) {
            Some(r) => Value::Int(r),
            None => Value::int(a.as_bigint().unwrap().abs()),
        }),
        _ => Err(PyErr::new(ExcKind::TypeError, format!("bad operand type for abs(): '{}'", a.type_name()))),
    }
}

// ---------------------------------------------------------------------------------------------
// Conversions: int(x), float(x)

/// Strips Python whitespace (for `str`) or ASCII whitespace (for `bytes`), then removes the
/// underscores Python allows between digits; `None` if an underscore is misplaced.
fn numeric_text(s: &str) -> Option<String> {
    let t = s.trim_matches(|c: char| super::ctype::py_isspace(c as u32));
    if !t.contains('_') {
        return Some(t.to_string());
    }
    let b = t.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'_' {
            let ok = i > 0 && i + 1 < b.len() && b[i - 1].is_ascii_alphanumeric() && b[i + 1].is_ascii_alphanumeric();
            if !ok {
                return None;
            }
        }
    }
    Some(t.replace('_', ""))
}

fn parse_py_int(text: &str) -> Option<Value> {
    let t = numeric_text(text)?;
    let digits = t.strip_prefix(['+', '-']).unwrap_or(&t);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(match t.parse::<i64>() {
        Ok(i) => Value::Int(i),
        Err(_) => Value::int(t.trim_start_matches('+').parse::<BigInt>().ok()?),
    })
}

fn parse_py_float(text: &str) -> Option<f64> {
    let t = numeric_text(text)?;
    let body = t.strip_prefix(['+', '-']).unwrap_or(&t);
    let lower = body.to_ascii_lowercase();
    if lower == "inf" || lower == "infinity" || lower == "nan" {
        return t.to_ascii_lowercase().parse::<f64>().ok();
    }
    // Python's grammar: digits, optional fraction, optional exponent; nothing else.
    let ok =
        !body.is_empty() && body.bytes().all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'));
    if !ok {
        return None;
    }
    t.parse::<f64>().ok()
}

fn text_of(x: &Value) -> Option<String> {
    match x {
        Value::Str(s) => Some(s.to_string()),
        // int()/float() of bytes only accept ASCII; non-ASCII can never parse.
        Value::Bytes(b) => Some(
            b.iter()
                .map(|&c| if c.is_ascii() { c as char } else { '\u{FFFD}' })
                .collect::<String>()
                .trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'))
                .to_string(),
        ),
        _ => None,
    }
}

/// `int(x)` where the port needs it in i64: `OverflowError` for an int beyond i64
/// (DIVERGE(bigint): Python goes on with the big int).
pub fn py_int(x: &Value) -> PyResult<i64> {
    let v = py_int_obj(x)?;
    v.as_int().ok_or_else(|| PyErr::new(ExcKind::OverflowError, "int too large for the port (i64)"))
}

/// `int(x)`, exact.
pub fn py_int_obj(x: &Value) -> PyResult<Value> {
    match x {
        Value::Int(_) | Value::BigInt(_) => Ok(x.clone()),
        Value::Bool(b) => Ok(Value::Int(*b as i64)),
        Value::Float(f) => float_to_int_obj(*f),
        Value::Str(_) | Value::Bytes(_) => {
            let t = text_of(x).unwrap();
            parse_py_int(&t).ok_or_else(|| {
                PyErr::new(ExcKind::ValueError, format!("invalid literal for int() with base 10: {}", x.repr()))
            })
        }
        _ => Err(PyErr::new(
            ExcKind::TypeError,
            format!("int() argument must be a string, a bytes-like object or a real number, not '{}'", x.type_name()),
        )),
    }
}

/// `int(f)` of a float where the port needs it in i64 (see [`py_int`]).
pub fn float_to_int(f: f64) -> PyResult<i64> {
    let v = float_to_int_obj(f)?;
    v.as_int().ok_or_else(|| PyErr::new(ExcKind::OverflowError, "int too large for the port (i64)"))
}

/// `int(f)` of a float: truncation, `ValueError` for NaN, `OverflowError` for infinities.
pub fn float_to_int_obj(f: f64) -> PyResult<Value> {
    if f.is_nan() {
        return Err(PyErr::new(ExcKind::ValueError, "cannot convert float NaN to integer"));
    }
    if f.is_infinite() {
        return Err(PyErr::new(ExcKind::OverflowError, "cannot convert float infinity to integer"));
    }
    let t = f.trunc();
    if t >= 9.223_372_036_854_776e18 || t < -9.223_372_036_854_776e18 {
        return Ok(Value::int(BigInt::from_f64_exact(t)));
    }
    Ok(Value::Int(t as i64))
}

/// The exact int of an integral float.
trait FromF64Exact {
    fn from_f64_exact(f: f64) -> BigInt;
}

impl FromF64Exact for BigInt {
    fn from_f64_exact(f: f64) -> BigInt {
        num_traits::FromPrimitive::from_f64(f).expect("finite float")
    }
}

/// `float(x)`.
pub fn py_float(x: &Value) -> PyResult<f64> {
    match x {
        Value::Float(f) => Ok(*f),
        Value::Int(i) => Ok(*i as f64),
        Value::BigInt(b) => {
            let f = bigint_to_f64(b);
            if f.is_infinite() {
                return Err(PyErr::new(ExcKind::OverflowError, "int too large to convert to float"));
            }
            Ok(f)
        }
        Value::Bool(b) => Ok(*b as i64 as f64),
        Value::Str(_) | Value::Bytes(_) => {
            let t = text_of(x).unwrap();
            parse_py_float(&t).ok_or_else(|| {
                PyErr::new(ExcKind::ValueError, format!("could not convert string to float: {}", x.repr()))
            })
        }
        _ => Err(PyErr::new(
            ExcKind::TypeError,
            format!("float() argument must be a string or a real number, not '{}'", x.type_name()),
        )),
    }
}

/// The `ValueError` of `a, b, ... = x` with more than `n` items: Python 3.14 adds the count for
/// a `list`, `tuple` or `dict` (not for `bytes`, `str` or iterators).
pub fn too_many_values(x: &Value, n: usize, got: usize) -> PyErr {
    let msg = if matches!(x, Value::List(_) | Value::Tuple(_) | Value::Dict(_)) {
        format!("too many values to unpack (expected {n}, got {got})")
    } else {
        format!("too many values to unpack (expected {n})")
    };
    PyErr::new(ExcKind::ValueError, msg)
}

// ---------------------------------------------------------------------------------------------
// min / max with Python's NaN behaviour

/// `min(a, b)`: keeps the first unless the second is strictly smaller (so NaN handling follows
/// argument order, like Python and unlike `f64::min`).
#[inline]
pub fn pymin(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// `max(a, b)`: keeps the first unless the second is strictly greater.
#[inline]
pub fn pymax(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// `min(xs)` of a non-empty sequence.
pub fn pymin_iter(xs: impl IntoIterator<Item = f64>) -> f64 {
    let mut it = xs.into_iter();
    let first = it.next().expect("min() arg is an empty sequence");
    it.fold(first, pymin)
}

/// `max(xs)` of a non-empty sequence.
pub fn pymax_iter(xs: impl IntoIterator<Item = f64>) -> f64 {
    let mut it = xs.into_iter();
    let first = it.next().expect("max() arg is an empty sequence");
    it.fold(first, pymax)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_and_float_conversions() {
        assert_eq!(py_int(&Value::bytes(b" 12 ")).unwrap(), 12);
        assert_eq!(py_int(&Value::bytes(b"+1_000")).unwrap(), 1000);
        assert!(py_int(&Value::bytes(b"1.5")).is_err());
        assert_eq!(py_int(&Value::Float(-2.7)).unwrap(), -2);
        assert!(py_int(&Value::Float(f64::INFINITY)).unwrap_err().is(ExcKind::OverflowError));
        assert_eq!(py_float(&Value::bytes(b"1e3")).unwrap(), 1000.0);
        assert_eq!(py_float(&Value::str("-.5")).unwrap(), -0.5);
        assert!(py_float(&Value::str(".")).is_err());
        assert!(py_float(&Value::str("0x10")).is_err());
        assert!(py_float(&Value::str("nan")).unwrap().is_nan());
        assert!(py_int(&Value::None).unwrap_err().is(ExcKind::TypeError));
    }

    #[test]
    fn numeric_equality_and_order() {
        assert!(py_eq(&Value::Int(1), &Value::Float(1.0)));
        assert!(py_eq(&Value::Bool(true), &Value::Int(1)));
        assert!(!py_eq(&Value::Float(f64::NAN), &Value::Float(f64::NAN)));
        assert!(py_lt(&Value::Int(1), &Value::Float(1.5)).unwrap());
        assert!(py_lt(&Value::Int(9007199254740993), &Value::Float(9007199254740992.0)).is_ok());
        assert!(!py_lt(&Value::Int(9007199254740993), &Value::Float(9007199254740992.0)).unwrap());
        assert!(py_lt(&Value::bytes(b"a"), &Value::Int(1)).is_err());
    }

    #[test]
    fn ints_beyond_i64_are_exact() {
        let big = |s: &str| Value::int(s.parse::<BigInt>().unwrap());
        let b = big("99999999999999999999");
        assert_eq!(b.repr(), "99999999999999999999");
        assert_eq!(b.type_name(), "int");
        assert!(b.is_int() && b.as_int().is_none());
        assert_eq!(py_add(&b, &Value::Int(1)).unwrap().repr(), "100000000000000000000");
        assert_eq!(py_sub(&py_add(&b, &Value::Int(1)).unwrap(), &b).unwrap().repr(), "1");
        assert_eq!(py_add(&Value::Int(i64::MAX), &Value::Int(1)).unwrap().repr(), "9223372036854775808");
        assert_eq!(py_neg(&Value::Int(i64::MIN)).unwrap().repr(), "9223372036854775808");
        assert_eq!(py_floordiv(&py_neg(&b).unwrap(), &Value::Int(7)).unwrap().repr(), "-14285714285714285715");
        assert_eq!(py_mod(&py_neg(&b).unwrap(), &Value::Int(7)).unwrap().repr(), "6");
        assert_eq!(py_float(&b).unwrap(), 1e20);
        assert_eq!(py_float(&Value::Float(1.5)).unwrap(), 1.5);
        assert!(py_eq(&big("100000000000000000000"), &Value::Float(1e20)));
        assert!(py_lt(&b, &Value::Float(1e20)).unwrap());
        assert!(py_lt(&Value::Int(5), &b).unwrap());
        let huge = big(&format!("1{}", "0".repeat(400)));
        assert!(py_float(&huge).unwrap_err().is(ExcKind::OverflowError));
        assert!(py_add(&huge, &Value::Float(1.0)).unwrap_err().is(ExcKind::OverflowError));
        assert!(py_lt(&Value::Float(f64::MAX), &huge).unwrap());
        assert_eq!(py_int_obj(&Value::Float(1e20)).unwrap().repr(), "100000000000000000000");
        assert_eq!(py_int_obj(&Value::str("-99999999999999999999")).unwrap().repr(), "-99999999999999999999");
        assert!(py_int(&b).unwrap_err().is(ExcKind::OverflowError));
        assert!(py_mul(&Value::bytes(b"x"), &b).unwrap_err().is(ExcKind::OverflowError));
        assert_eq!(py_mul(&Value::bytes(b"x"), &py_neg(&b).unwrap()).unwrap().repr(), "b''");
        assert_eq!(py_truediv(&b, &Value::Int(4)).unwrap().repr(), "2.5e+19");
    }

    #[test]
    fn division_and_modulo() {
        assert!(py_truediv(&Value::Float(1.0), &Value::Float(0.0)).unwrap_err().is(ExcKind::ZeroDivisionError));
        assert_eq!(int_floordiv(-7, 2), -4);
        assert_eq!(int_mod(-7, 2), 1);
        assert_eq!(float_mod(-7.5, 2.0), 0.5);
        assert_eq!(float_floordiv(-7.5, 2.0), -4.0);
    }

    #[test]
    fn min_max_follow_argument_order() {
        assert!(pymin(f64::NAN, 1.0).is_nan());
        assert_eq!(pymin(1.0, f64::NAN), 1.0);
        assert_eq!(pymax(0.0, -0.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(pymax(-0.0, 0.0).to_bits(), (-0.0f64).to_bits());
    }
}
