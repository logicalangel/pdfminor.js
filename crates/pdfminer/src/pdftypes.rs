//! Port of `pdfminer/pdftypes.py`.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use log::warn;

use crate::ascii85::{ascii85decode, asciihexdecode};
use crate::ccitt::ccittfaxdecode;
use crate::lzw::lzwdecode;
use crate::pdfdocument::PDFDocument;
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::bytes_repr;
use crate::pyrt::value::{DictRef, Value, dict_repr, py_eq};
use crate::pyrt::zlib;
use crate::runlength::rldecode;
use crate::settings;
use crate::utils::{apply_png_predictor, apply_tiff_predictor};

pub const LITERAL_CRYPT: &str = "Crypt";

// Abbreviation of Filter names in PDF 4.8.6. "Inline Images"
pub const LITERALS_FLATE_DECODE: &[&str] = &["FlateDecode", "Fl"];
pub const LITERALS_LZW_DECODE: &[&str] = &["LZWDecode", "LZW"];
pub const LITERALS_ASCII85_DECODE: &[&str] = &["ASCII85Decode", "A85"];
pub const LITERALS_ASCIIHEX_DECODE: &[&str] = &["ASCIIHexDecode", "AHx"];
pub const LITERALS_RUNLENGTH_DECODE: &[&str] = &["RunLengthDecode", "RL"];
pub const LITERALS_CCITTFAX_DECODE: &[&str] = &["CCITTFaxDecode", "CCF"];
pub const LITERALS_DCT_DECODE: &[&str] = &["DCTDecode", "DCT"];
pub const LITERALS_JBIG2_DECODE: &[&str] = &["JBIG2Decode"];
pub const LITERALS_JPX_DECODE: &[&str] = &["JPXDecode"];

/// `x in (LIT(a), LIT(b), ...)`: true when `x` is one of these interned literals.
pub fn in_literals(x: &Value, names: &[&str]) -> bool {
    match x {
        Value::Literal(l) => names.iter().any(|n| l.is(n)),
        _ => false,
    }
}

/// `DecipherCallable`: `decipher(objid, genno, data, attrs=None) -> bytes`.
pub type DecipherCallable = dyn Fn(i64, i64, &[u8], Option<&DictRef>) -> PyResult<Vec<u8>>;

struct ObjRefInner {
    doc: Option<Weak<PDFDocument>>,
    objid: i64,
}

/// `PDFObjRef`. Python compares refs by identity (no `__eq__`), so a ref is an `Rc` and
/// [`PDFObjRef::is`] compares pointers.
#[derive(Clone)]
pub struct PDFObjRef(Rc<ObjRefInner>);

impl PDFObjRef {
    /// `PDFObjRef(doc, objid)`.
    pub fn new(doc: Option<Weak<PDFDocument>>, objid: i64) -> PyResult<Self> {
        if objid == 0 && settings::STRICT {
            return Err(PyErr::new(ExcKind::PDFValueError, "PDF object id cannot be 0."));
        }
        Ok(PDFObjRef(Rc::new(ObjRefInner { doc, objid })))
    }

    pub fn objid(&self) -> i64 {
        self.0.objid
    }

    pub fn doc(&self) -> Option<Rc<PDFDocument>> {
        self.0.doc.as_ref().and_then(|w| w.upgrade())
    }

    pub fn is(&self, other: &PDFObjRef) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    pub fn repr(&self) -> String {
        format!("<PDFObjRef:{}>", self.0.objid)
    }

    /// `ref.resolve(default)`.
    pub fn resolve(&self, default: &Value) -> PyResult<Value> {
        let Some(doc) = self.doc() else {
            // `assert self.doc is not None`
            return Err(PyErr::new(ExcKind::AssertionError, ""));
        };
        match doc.getobj(self.0.objid) {
            Ok(v) => Ok(v),
            Err(e) if e.is(ExcKind::PDFObjectNotFound) => Ok(default.clone()),
            Err(e) => Err(e),
        }
    }
}

/// `resolve1(x)`: resolves an object (arrays and dicts may still hold references).
pub fn resolve1(x: &Value) -> PyResult<Value> {
    resolve1_default(x, &Value::None)
}

/// `resolve1(x, default)`.
pub fn resolve1_default(x: &Value, default: &Value) -> PyResult<Value> {
    let mut x = x.clone();
    while let Value::Ref(r) = &x {
        x = r.resolve(default)?;
    }
    Ok(x)
}

/// `resolve_all(x)`: recursively resolves the object and everything inside it. Dicts are updated
/// in place (as in Python); lists are rebuilt.
pub fn resolve_all(x: &Value) -> PyResult<Value> {
    resolve_all_default(x, &Value::None)
}

pub fn resolve_all_default(x: &Value, default: &Value) -> PyResult<Value> {
    let mut x = x.clone();
    while let Value::Ref(r) = &x {
        x = r.resolve(default)?;
    }
    match &x {
        Value::List(l) => {
            let items = l.borrow().clone();
            let mut out = Vec::with_capacity(items.len());
            for v in &items {
                out.push(resolve_all_default(v, default)?);
            }
            Ok(Value::list(out))
        }
        Value::Dict(d) => {
            let items: Vec<(Rc<str>, Value)> = d.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (k, v) in items {
                let r = resolve_all_default(&v, default)?;
                d.borrow_mut().insert(k, r);
            }
            Ok(x)
        }
        _ => Ok(x),
    }
}

/// `decipher_all(decipher, objid, genno, x)`: recursively deciphers the strings in an object.
pub fn decipher_all(decipher: &DecipherCallable, objid: i64, genno: i64, x: &Value) -> PyResult<Value> {
    match x {
        Value::Bytes(b) => {
            if b.is_empty() {
                return Ok(x.clone());
            }
            Ok(Value::bytes(&decipher(objid, genno, b, None)?))
        }
        Value::List(l) => {
            let items = l.borrow().clone();
            let mut out = Vec::with_capacity(items.len());
            for v in &items {
                out.push(decipher_all(decipher, objid, genno, v)?);
            }
            Ok(Value::list(out))
        }
        Value::Dict(d) => {
            let items: Vec<(Rc<str>, Value)> = d.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (k, v) in items {
                let r = decipher_all(decipher, objid, genno, &v)?;
                d.borrow_mut().insert(k, r);
            }
            Ok(x.clone())
        }
        _ => Ok(x.clone()),
    }
}

/// `int_value(x)`: the int (bools count as ints), or 0 in non-strict mode.
pub fn int_value(x: &Value) -> PyResult<i64> {
    let x = resolve1(x)?;
    match x.as_int() {
        Some(i) => Ok(i),
        None => {
            if settings::STRICT {
                return Err(PyErr::new(ExcKind::PDFTypeError, format!("Integer required: {}", x.repr())));
            }
            Ok(0)
        }
    }
}

/// `float_value(x)`: the float (ints are *not* accepted), or 0.0 in non-strict mode.
pub fn float_value(x: &Value) -> PyResult<f64> {
    let x = resolve1(x)?;
    match x {
        Value::Float(f) => Ok(f),
        _ => {
            if settings::STRICT {
                return Err(PyErr::new(ExcKind::PDFTypeError, format!("Float required: {}", x.repr())));
            }
            Ok(0.0)
        }
    }
}

/// `num_value(x)`: the number unchanged (an int stays an int), or the int 0 in non-strict mode.
pub fn num_value(x: &Value) -> PyResult<Value> {
    let x = resolve1(x)?;
    if !x.is_number() {
        if settings::STRICT {
            return Err(PyErr::new(ExcKind::PDFTypeError, format!("Int or Float required: {}", x.repr())));
        }
        return Ok(Value::Int(0));
    }
    Ok(x)
}

/// `uint_value(x, n_bits)`: resolves a number and reads it as a two's-complement unsigned number.
pub fn uint_value(x: &Value, n_bits: u32) -> PyResult<i64> {
    let xi = int_value(x)?;
    if xi > 0 { Ok(xi) } else { Ok(xi + (1i64 << n_bits)) }
}

/// `str_value(x)`: the bytes, or `b""` in non-strict mode.
pub fn str_value(x: &Value) -> PyResult<Rc<[u8]>> {
    let x = resolve1(x)?;
    match x {
        Value::Bytes(b) => Ok(b),
        _ => {
            if settings::STRICT {
                return Err(PyErr::new(ExcKind::PDFTypeError, format!("String required: {}", x.repr())));
            }
            Ok(Rc::from(&b""[..]))
        }
    }
}

/// `list_value(x)`: the items of a list or tuple (a snapshot), or `[]` in non-strict mode.
pub fn list_value(x: &Value) -> PyResult<Vec<Value>> {
    let x = resolve1(x)?;
    match x.sequence_items() {
        Some(items) => Ok(items),
        None => {
            if settings::STRICT {
                return Err(PyErr::new(ExcKind::PDFTypeError, format!("List required: {}", x.repr())));
            }
            Ok(Vec::new())
        }
    }
}

/// `dict_value(x)`: the dict (shared, as in Python), or a new `{}` in non-strict mode.
pub fn dict_value(x: &Value) -> PyResult<DictRef> {
    let x = resolve1(x)?;
    match x {
        Value::Dict(d) => Ok(d),
        _ => {
            if settings::STRICT {
                log::error!("PDFTypeError : Dict required: {}", x.repr());
                return Err(PyErr::new(ExcKind::PDFTypeError, format!("Dict required: {}", x.repr())));
            }
            Ok(Value::new_dict())
        }
    }
}

/// `stream_value(x)`: the stream, or a new empty `PDFStream({}, b"")` in non-strict mode.
pub fn stream_value(x: &Value) -> PyResult<Rc<PDFStream>> {
    let x = resolve1(x)?;
    match x {
        Value::Stream(s) => Ok(s),
        _ => {
            if settings::STRICT {
                return Err(PyErr::new(ExcKind::PDFTypeError, format!("PDFStream required: {}", x.repr())));
            }
            Ok(Rc::new(PDFStream::new(Value::new_dict(), Rc::from(&b""[..]), None)))
        }
    }
}

/// `decompress_corrupted(data)`: decodes data that `zlib.decompress` rejected (usually for a bad
/// checksum) one byte at a time, keeping what was decoded before an error in the last 3 bytes.
pub fn decompress_corrupted(data: &[u8]) -> PyResult<Vec<u8>> {
    let mut d = zlib::DecompressObj::new();
    let mut result_str: Vec<u8> = Vec::new();
    let mut i = 0usize;
    // `buffer = f.read(1)` ... `while buffer:`
    while i < data.len() {
        match d.decompress(&data[i..i + 1]) {
            Ok(out) => result_str.extend(out),
            Err(e) => {
                // Let the error propagate if we're not yet in the CRC checksum
                if (i as i64) < data.len() as i64 - 3 {
                    return Err(e);
                }
                warn!("Data-loss while decompressing corrupted data");
                return Ok(result_str);
            }
        }
        i += 1;
    }
    Ok(result_str)
}

/// `"name" in container` for the objects a filter's `params` can be.
pub fn py_contains_str(container: &Value, name: &str) -> PyResult<bool> {
    match container {
        Value::Dict(d) => Ok(d.borrow().contains_key(name)),
        Value::Stream(s) => Ok(s.contains(name)),
        Value::List(l) => Ok(l.borrow().iter().any(|v| py_eq(v, &Value::str(name)))),
        Value::Tuple(t) => Ok(t.iter().any(|v| py_eq(v, &Value::str(name)))),
        Value::Str(s) => Ok(s.contains(name)),
        Value::Bytes(_) => Err(PyErr::new(ExcKind::TypeError, "a bytes-like object is required, not 'str'")),
        other => {
            Err(PyErr::new(ExcKind::TypeError, format!("argument of type '{}' is not iterable", other.type_name())))
        }
    }
}

/// `container["name"]` for the objects a filter's `params` can be.
pub fn py_getitem_str(container: &Value, name: &str) -> PyResult<Value> {
    match container {
        Value::Dict(d) => {
            d.borrow().get(name).cloned().ok_or_else(|| PyErr::new(ExcKind::KeyError, crate::pyrt::fmt::str_repr(name)))
        }
        Value::Stream(s) => s.getitem(name),
        Value::List(_) | Value::Tuple(_) | Value::Str(_) | Value::Bytes(_) => Err(PyErr::new(
            ExcKind::TypeError,
            format!("{} indices must be integers or slices, not str", container.type_name()),
        )),
        other => Err(PyErr::new(ExcKind::TypeError, format!("'{}' object is not subscriptable", other.type_name()))),
    }
}

/// `container.get("name", default)` for the objects that have `.get` (dicts and streams).
pub fn py_get_str(container: &Value, name: &str, default: Value) -> PyResult<Value> {
    match container {
        Value::Dict(d) => Ok(d.borrow().get(name).cloned().unwrap_or(default)),
        Value::Stream(s) => Ok(s.get(name).unwrap_or(default)),
        other => {
            Err(PyErr::new(ExcKind::AttributeError, format!("'{}' object has no attribute 'get'", other.type_name())))
        }
    }
}

/// `PDFStream`.
pub struct PDFStream {
    pub attrs: DictRef,
    pub rawdata: RefCell<Option<Rc<[u8]>>>,
    pub decipher: Option<Rc<DecipherCallable>>,
    pub data: RefCell<Option<Rc<[u8]>>>,
    pub objid: Cell<Option<i64>>,
    pub genno: Cell<Option<i64>>,
}

impl PDFStream {
    pub fn new(attrs: DictRef, rawdata: Rc<[u8]>, decipher: Option<Rc<DecipherCallable>>) -> Self {
        PDFStream {
            attrs,
            rawdata: RefCell::new(Some(rawdata)),
            decipher,
            data: RefCell::new(None),
            objid: Cell::new(None),
            genno: Cell::new(None),
        }
    }

    pub fn set_objid(&self, objid: i64, genno: i64) {
        self.objid.set(Some(objid));
        self.genno.set(Some(genno));
    }

    pub fn repr(&self) -> String {
        let objid = self.objid.get().map_or("None".to_string(), |o| o.to_string());
        match &*self.data.borrow() {
            None => {
                let raw = self.rawdata.borrow();
                let n = raw.as_ref().map_or(0, |r| r.len());
                format!("<PDFStream({}): raw={}, {}>", objid, n, dict_repr(&self.attrs.borrow()))
            }
            Some(data) => {
                format!("<PDFStream({}): len={}, {}>", objid, data.len(), dict_repr(&self.attrs.borrow()))
            }
        }
    }

    /// `name in stream`.
    pub fn contains(&self, name: &str) -> bool {
        self.attrs.borrow().contains_key(name)
    }

    /// `stream[name]`: `PDFKeyError` when missing.
    pub fn getitem(&self, name: &str) -> PyResult<Value> {
        self.attrs.borrow().get(name).cloned().ok_or_else(|| {
            PyErr::new(
                ExcKind::PDFKeyError,
                format!(
                    "PDF stream object {} does not have attribute '{}'",
                    self.objid.get().map_or("None".to_string(), |o| o.to_string()),
                    name
                ),
            )
        })
    }

    /// `stream.get(name)` (`None` for the default `None`).
    pub fn get(&self, name: &str) -> Option<Value> {
        self.attrs.borrow().get(name).cloned()
    }

    /// `stream.get_any(names, default)`.
    pub fn get_any(&self, names: &[&str], default: Value) -> Value {
        let attrs = self.attrs.borrow();
        for name in names {
            if let Some(v) = attrs.get(*name) {
                return v.clone();
            }
        }
        default
    }

    /// `stream.get_filters()`: `(filter, params)` pairs, resolved.
    pub fn get_filters(&self) -> PyResult<Vec<(Value, Value)>> {
        let filters = resolve1(&self.get_any(&["F", "Filter"], Value::list(vec![])))?;
        let params = resolve1(&self.get_any(&["DP", "DecodeParms", "FDecodeParms"], Value::dict(Default::default())))?;
        if !filters.truthy() {
            return Ok(vec![]);
        }
        let filters: Vec<Value> = match &filters {
            Value::List(l) => l.borrow().clone(),
            other => vec![other.clone()],
        };
        let params: Vec<Value> = match &params {
            Value::List(l) => l.borrow().clone(),
            // Make sure the parameters list is the same as filters.
            other => vec![other.clone(); filters.len()],
        };
        if settings::STRICT && params.len() != filters.len() {
            return Err(PyErr::new(ExcKind::PDFException, "Parameters len filter mismatch"));
        }
        let mut resolved_filters = Vec::with_capacity(filters.len());
        for f in &filters {
            resolved_filters.push(resolve1(f)?);
        }
        let mut resolved_params = Vec::with_capacity(params.len());
        for p in &params {
            resolved_params.push(resolve1(p)?);
        }
        Ok(resolved_filters.into_iter().zip(resolved_params).collect())
    }

    /// `stream.decode()`.
    pub fn decode(&self) -> PyResult<()> {
        let rawdata = self.rawdata.borrow().clone();
        // assert self.data is None and self.rawdata is not None
        let Some(rawdata) = rawdata else {
            return Err(PyErr::new(ExcKind::AssertionError, "stream already decoded"));
        };
        let mut data: Rc<[u8]> = rawdata;
        if let Some(decipher) = &self.decipher {
            // Handle encryption
            let (Some(objid), Some(genno)) = (self.objid.get(), self.genno.get()) else {
                return Err(PyErr::new(ExcKind::AssertionError, ""));
            };
            data = Rc::from(decipher(objid, genno, &data, Some(&self.attrs))?);
        }
        let filters = self.get_filters()?;
        if filters.is_empty() {
            *self.data.borrow_mut() = Some(data);
            *self.rawdata.borrow_mut() = None;
            return Ok(());
        }
        for (f, params) in filters {
            if in_literals(&f, LITERALS_FLATE_DECODE) {
                // will get errors if the document is encrypted.
                match zlib::decompress(&data) {
                    Ok(d) => data = Rc::from(d),
                    Err(e) => {
                        if settings::STRICT {
                            let error_msg = format!("Invalid zlib bytes: {}, {}", e, bytes_repr(&data));
                            return Err(PyErr::new(ExcKind::PDFException, error_msg));
                        }
                        data = match decompress_corrupted(&data) {
                            Ok(d) => Rc::from(d),
                            Err(e) if e.is(ExcKind::ZlibError) => Rc::from(&b""[..]),
                            Err(e) => return Err(e),
                        };
                    }
                }
            } else if in_literals(&f, LITERALS_LZW_DECODE) {
                data = Rc::from(lzwdecode(&data)?);
            } else if in_literals(&f, LITERALS_ASCII85_DECODE) {
                data = Rc::from(ascii85decode(&data)?);
            } else if in_literals(&f, LITERALS_ASCIIHEX_DECODE) {
                data = Rc::from(asciihexdecode(&data)?);
            } else if in_literals(&f, LITERALS_RUNLENGTH_DECODE) {
                data = Rc::from(rldecode(&data)?);
            } else if in_literals(&f, LITERALS_CCITTFAX_DECODE) {
                data = Rc::from(ccittfaxdecode(&data, &params)?);
            } else if in_literals(&f, LITERALS_DCT_DECODE) {
                // This is probably a JPG stream
                // it does not need to be decoded twice.
                // Just return the stream to the user.
            } else if in_literals(&f, LITERALS_JBIG2_DECODE) || in_literals(&f, LITERALS_JPX_DECODE) {
            } else if in_literals(&f, &[LITERAL_CRYPT]) {
                // not yet..
                return Err(PyErr::new(ExcKind::PDFNotImplementedError, "/Crypt filter is unsupported"));
            } else {
                return Err(PyErr::new(ExcKind::PDFNotImplementedError, format!("Unsupported filter: {}", f.repr())));
            }
            // apply predictors
            if params.truthy() && py_contains_str(&params, "Predictor")? {
                let pred = int_value(&py_getitem_str(&params, "Predictor")?)?;
                if pred == 1 {
                    // no predictor
                } else if pred == 2 {
                    // TIFF predictor 2
                    let colors = int_value(&py_get_str(&params, "Colors", Value::Int(1))?)?;
                    let columns = int_value(&py_get_str(&params, "Columns", Value::Int(1))?)?;
                    let raw_bits_per_component = py_get_str(&params, "BitsPerComponent", Value::Int(8))?;
                    let bitspercomponent = int_value(&raw_bits_per_component)?;
                    data = Rc::from(apply_tiff_predictor(colors, columns, bitspercomponent, &data)?);
                } else if pred >= 10 {
                    // PNG predictor
                    let colors = int_value(&py_get_str(&params, "Colors", Value::Int(1))?)?;
                    let columns = int_value(&py_get_str(&params, "Columns", Value::Int(1))?)?;
                    let raw_bits_per_component = py_get_str(&params, "BitsPerComponent", Value::Int(8))?;
                    let bitspercomponent = int_value(&raw_bits_per_component)?;
                    data = Rc::from(apply_png_predictor(pred, colors, columns, bitspercomponent, &data)?);
                } else {
                    let error_msg = format!("Unsupported predictor: {}", pred);
                    return Err(PyErr::new(ExcKind::PDFNotImplementedError, error_msg));
                }
            }
        }
        *self.data.borrow_mut() = Some(data);
        *self.rawdata.borrow_mut() = None;
        Ok(())
    }

    /// `stream.get_data()`.
    pub fn get_data(&self) -> PyResult<Rc<[u8]>> {
        if self.data.borrow().is_none() {
            self.decode()?;
        }
        Ok(self.data.borrow().clone().expect("decoded"))
    }

    /// `stream.get_rawdata()`.
    pub fn get_rawdata(&self) -> Option<Rc<[u8]>> {
        self.rawdata.borrow().clone()
    }
}
