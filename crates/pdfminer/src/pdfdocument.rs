//! Port of `pdfminer/pdfdocument.py`.
//!
//! Object ids: Python keys the xref tables and the object cache by whatever object the file
//! provides (normally an `int`, but a broken object stream can put a float, a name or a keyword
//! where an object id belongs, and an `/Index` array of floats makes `start + i` a float). The port
//! keeps object ids as [`Value`]s with Python's `dict` key semantics ([`ObjKey`]), so lookups
//! fail (or succeed) exactly where Python's do. [`PDFDocument::getobj`] takes the usual `i64`.
//!
//! The parser is shared like in Python: resolving a reference while the document's parser is in
//! the middle of `nextobject()` (an indirect `/Length`, or a stream dictionary given by
//! reference) re-enters the document. Python reuses the same parser for the nested object and the
//! interrupted `do_keyword` seeks it back right after; the port parses the nested object with a
//! fresh parser over the same file instead, which leaves the same state behind (every path that
//! continues after such a nested call starts with a `seek`).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::{Rc, Weak};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use indexmap::IndexMap;
use log::{debug, warn};
use md5::{Digest, Md5};
use sha2::{Sha256, Sha384, Sha512};

use crate::_saslprep::saslprep;
use crate::arcfour::Arcfour;
use crate::casting::safe_int;
use crate::data_structures::NumberTree;
use crate::pdfparser::{PDFParser, PDFStreamParser};
use crate::pdftypes::{
    DecipherCallable, PDFObjRef, PDFStream, decipher_all, dict_value, int_value, list_value, py_getitem_str, str_value,
    stream_value, uint_value,
};
use crate::psparser::{PSKeyword, PSLiteral, PSStackParserImpl, RevReadLines, literal_name};
use crate::pyrt::ctype::{bytes_isdigit, bytes_strip, py_isspace};
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::{bytes_repr, str_repr};
use crate::pyrt::io::BytesIO;
use crate::pyrt::value::{DictRef, Value, dict_repr, py_add, py_cmp, py_eq, py_int, py_lt, py_mul, py_sub};
use crate::settings;
use crate::utils::{choplist, decode_text, format_int_alpha, format_int_roman, nunpack, unpad_aes};

/// Python's recursion limit, as the port applies it to recursion that the input controls (a
/// `/Prev` chain, nested object streams, outline siblings, page and number trees).
// DIVERGE(recursion-depth): Python raises `RecursionError` when its whole stack reaches 1000
// frames, so the exact nesting depth depends on the caller; the port uses a fixed budget.
pub const RECURSION_LIMIT: usize = 990;

fn recursion_error() -> PyErr {
    PyErr::new(ExcKind::RecursionError, "maximum recursion depth exceeded")
}

// some predefined literals and keywords.
pub const LITERAL_OBJSTM: &str = "ObjStm";
pub const LITERAL_XREF: &str = "XRef";
pub const LITERAL_CATALOG: &str = "Catalog";

/// `x is LIT(name)`.
fn is_lit(x: &Value, name: &str) -> bool {
    matches!(x, Value::Literal(l) if l.is(name))
}

/// `x is KWD(name)`.
fn is_kwd(x: &Value, name: &[u8]) -> bool {
    matches!(x, Value::Keyword(k) if k.is(name))
}

/// `d.get(key)` on a shared dict, cloned out of the borrow.
fn dget(d: &DictRef, key: &str) -> Option<Value> {
    d.borrow().get(key).cloned()
}

/// `d[key]` on a `dict` (plain `KeyError`).
fn ditem(d: &DictRef, key: &str) -> PyResult<Value> {
    dget(d, key).ok_or_else(|| PyErr::new(ExcKind::KeyError, str_repr(key)))
}

fn dcontains(d: &DictRef, key: &str) -> bool {
    d.borrow().contains_key(key)
}

// ---------------------------------------------------------------------------------------------
// Python helpers: hashing of object ids, len(), iteration, slicing, struct.pack

/// A Python object as a `dict`/`set` key: equal keys are the ones Python's `dict` treats as the
/// same (`1 == 1.0 == True`), and `list`/`dict` are unhashable.
#[derive(Clone)]
pub enum ObjKey {
    None,
    Int(i64),
    /// A float that is not equal to any `i64` (its bits).
    Float(u64),
    Bytes(Rc<[u8]>),
    Str(Rc<str>),
    Literal(PSLiteral),
    Keyword(PSKeyword),
    /// Hashed by identity.
    Ref(PDFObjRef),
    /// Other objects hashed by identity (streams, tuples).
    Ptr(usize),
}

impl PartialEq for ObjKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ObjKey::None, ObjKey::None) => true,
            (ObjKey::Int(a), ObjKey::Int(b)) => a == b,
            (ObjKey::Float(a), ObjKey::Float(b)) => a == b,
            (ObjKey::Bytes(a), ObjKey::Bytes(b)) => a == b,
            (ObjKey::Str(a), ObjKey::Str(b)) => a == b,
            (ObjKey::Literal(a), ObjKey::Literal(b)) => a == b,
            (ObjKey::Keyword(a), ObjKey::Keyword(b)) => a == b,
            (ObjKey::Ref(a), ObjKey::Ref(b)) => a.is(b),
            (ObjKey::Ptr(a), ObjKey::Ptr(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for ObjKey {}

impl Hash for ObjKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            ObjKey::None => {}
            ObjKey::Int(i) => i.hash(state),
            ObjKey::Float(b) => b.hash(state),
            ObjKey::Bytes(b) => b.hash(state),
            ObjKey::Str(s) => s.hash(state),
            ObjKey::Literal(l) => l.hash(state),
            ObjKey::Keyword(k) => k.hash(state),
            ObjKey::Ref(r) => r.objid().hash(state),
            ObjKey::Ptr(p) => p.hash(state),
        }
    }
}

/// `hash(x)` as a key: `TypeError` for unhashable objects.
pub fn obj_key(x: &Value) -> PyResult<ObjKey> {
    Ok(match x {
        Value::None => ObjKey::None,
        Value::Bool(b) => ObjKey::Int(*b as i64),
        Value::Int(i) => ObjKey::Int(*i),
        Value::Float(f) => {
            let f = *f;
            if f.fract() == 0.0 && (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&f) {
                ObjKey::Int(f as i64)
            } else {
                ObjKey::Float(f.to_bits())
            }
        }
        Value::Bytes(b) => ObjKey::Bytes(b.clone()),
        Value::Str(s) => ObjKey::Str(s.clone()),
        Value::Literal(l) => ObjKey::Literal(l.clone()),
        Value::Keyword(k) => ObjKey::Keyword(k.clone()),
        Value::Ref(r) => ObjKey::Ref(r.clone()),
        Value::Stream(s) => ObjKey::Ptr(Rc::as_ptr(s) as *const u8 as usize),
        Value::Tuple(t) => ObjKey::Ptr(t.as_ptr() as usize),
        Value::List(_) | Value::Dict(_) => {
            return Err(PyErr::new(ExcKind::TypeError, format!("unhashable type: '{}'", x.type_name())));
        }
    })
}

/// `len(x)`.
fn py_len(x: &Value) -> PyResult<usize> {
    match x {
        Value::List(l) => Ok(l.borrow().len()),
        Value::Tuple(t) => Ok(t.len()),
        Value::Bytes(b) => Ok(b.len()),
        Value::Str(s) => Ok(s.chars().count()),
        Value::Dict(d) => Ok(d.borrow().len()),
        other => Err(PyErr::new(ExcKind::TypeError, format!("object of type '{}' has no len()", other.type_name()))),
    }
}

/// The items of `iter(x)` (all of them; the objects pdfminer iterates are finite).
pub(crate) fn py_iter_items(x: &Value) -> PyResult<Vec<Value>> {
    match x {
        Value::List(l) => Ok(l.borrow().clone()),
        Value::Tuple(t) => Ok(t.to_vec()),
        Value::Bytes(b) => Ok(b.iter().map(|&c| Value::Int(c as i64)).collect()),
        Value::Str(s) => Ok(s.chars().map(|c| Value::str(&c.to_string())).collect()),
        Value::Dict(d) => Ok(d.borrow().keys().map(|k| Value::Str(k.clone())).collect()),
        // A PDFStream has __getitem__ but no __iter__: iteration calls stream[0], which raises
        // PDFKeyError.
        Value::Stream(s) => Err(s.getitem("0").unwrap_err()),
        other => Err(PyErr::new(ExcKind::TypeError, format!("'{}' object is not iterable", other.type_name()))),
    }
}

/// `range(x)`'s argument check: an `int` (or `bool`).
fn range_arg(x: &Value) -> PyResult<i64> {
    x.as_int().ok_or_else(|| {
        PyErr::new(ExcKind::TypeError, format!("'{}' object cannot be interpreted as an integer", x.type_name()))
    })
}

/// A slice index (`None` or an int).
fn slice_index(x: Option<&Value>) -> PyResult<Option<i64>> {
    match x {
        None | Some(Value::None) => Ok(None),
        Some(v) => v.as_int().map(Some).ok_or_else(|| {
            PyErr::new(ExcKind::TypeError, "slice indices must be integers or None or have an __index__ method")
        }),
    }
}

/// `b[start:stop]` with Python's clamping and negative indices.
fn py_slice(b: &[u8], start: Option<i64>, stop: Option<i64>) -> &[u8] {
    let len = b.len() as i64;
    let norm = |i: i64| -> i64 { if i < 0 { (i + len).max(0) } else { i.min(len) } };
    let s = start.map_or(0, norm);
    let e = stop.map_or(len, norm);
    if s >= e { &b[0..0] } else { &b[s as usize..e as usize] }
}

/// `b[start:stop]` with `Value` indices.
fn py_slice_v<'a>(b: &'a [u8], start: Option<&Value>, stop: Option<&Value>) -> PyResult<&'a [u8]> {
    Ok(py_slice(b, slice_index(start)?, slice_index(stop)?))
}

/// `struct.pack("<L", v)`.
fn pack_l(v: i64) -> PyResult<[u8; 4]> {
    if !(0..=0xFFFF_FFFF).contains(&v) {
        return Err(PyErr::new(ExcKind::StructError, "'L' format requires 0 <= number <= 4294967295"));
    }
    Ok((v as u32).to_le_bytes())
}

/// `hashlib.<h>.update(x)` for an object from the PDF: only `bytes` are accepted.
fn hash_bytes(x: &Value) -> PyResult<&[u8]> {
    match x {
        Value::Bytes(b) => Ok(b),
        _ => Err(PyErr::new(ExcKind::TypeError, "object supporting the buffer API required")),
    }
}

/// `seq[i]` on a list with Python's negative indices.
fn list_index<'a>(seq: &'a [Value], i: &Value) -> PyResult<&'a Value> {
    let Some(i) = i.as_int() else {
        return Err(PyErr::new(
            ExcKind::TypeError,
            format!("list indices must be integers or slices, not {}", i.type_name()),
        ));
    };
    let len = seq.len() as i64;
    let j = if i < 0 { i + len } else { i };
    if j < 0 || j >= len {
        return Err(PyErr::new(ExcKind::IndexError, "list index out of range"));
    }
    Ok(&seq[j as usize])
}

// ---------------------------------------------------------------------------------------------
// AES (the `cryptography` package's CBC decryptor/encryptor)

enum AnyAes {
    A128(aes::Aes128),
    A192(aes::Aes192),
    A256(aes::Aes256),
}

impl AnyAes {
    /// `algorithms.AES(key)`.
    fn new(key: &[u8]) -> PyResult<Self> {
        Ok(match key.len() {
            16 => AnyAes::A128(aes::Aes128::new(GenericArray::from_slice(key))),
            24 => AnyAes::A192(aes::Aes192::new(GenericArray::from_slice(key))),
            32 => AnyAes::A256(aes::Aes256::new(GenericArray::from_slice(key))),
            n => return Err(PyErr::new(ExcKind::ValueError, format!("Invalid key size ({}) for AES.", n * 8))),
        })
    }

    fn decrypt_block(&self, b: &mut [u8]) {
        let b = GenericArray::from_mut_slice(b);
        match self {
            AnyAes::A128(c) => c.decrypt_block(b),
            AnyAes::A192(c) => c.decrypt_block(b),
            AnyAes::A256(c) => c.decrypt_block(b),
        }
    }

    fn encrypt_block(&self, b: &mut [u8]) {
        let b = GenericArray::from_mut_slice(b);
        match self {
            AnyAes::A128(c) => c.encrypt_block(b),
            AnyAes::A192(c) => c.encrypt_block(b),
            AnyAes::A256(c) => c.encrypt_block(b),
        }
    }
}

/// `Cipher(algorithms.AES(key), modes.CBC(iv))`: the key is checked first, then the IV.
fn aes_cbc(key: &[u8], iv: &[u8]) -> PyResult<AnyAes> {
    let cipher = AnyAes::new(key)?;
    if iv.len() != 16 {
        return Err(PyErr::new(ExcKind::ValueError, format!("Invalid IV size ({}) for CBC.", iv.len())));
    }
    Ok(cipher)
}

/// `Cipher(AES(key), CBC(iv)).decryptor().update(data)`: only whole blocks come out (the rest
/// stays buffered and is never finalized).
fn aes_cbc_decrypt_update(key: &[u8], iv: &[u8], data: &[u8]) -> PyResult<Vec<u8>> {
    let cipher = aes_cbc(key, iv)?;
    let n = data.len() / 16 * 16;
    let mut out = data[..n].to_vec();
    let mut prev = [0u8; 16];
    prev.copy_from_slice(iv);
    for block in out.chunks_mut(16) {
        let mut ct = [0u8; 16];
        ct.copy_from_slice(block);
        cipher.decrypt_block(block);
        for (b, p) in block.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        prev = ct;
    }
    Ok(out)
}

/// `encryptor.update(data) + encryptor.finalize()` in CBC mode.
fn aes_cbc_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> PyResult<Vec<u8>> {
    let cipher = aes_cbc(key, iv)?;
    if !data.len().is_multiple_of(16) {
        return Err(PyErr::new(
            ExcKind::ValueError,
            "The length of the provided data is not a multiple of the block length.",
        ));
    }
    let mut out = data.to_vec();
    let mut prev = [0u8; 16];
    prev.copy_from_slice(iv);
    for block in out.chunks_mut(16) {
        for (b, p) in block.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        cipher.encrypt_block(block);
        prev.copy_from_slice(block);
    }
    Ok(out)
}

fn md5_digest(parts: &[&[u8]]) -> Vec<u8> {
    let mut h = Md5::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().to_vec()
}

// ---------------------------------------------------------------------------------------------
// XRefs

/// `(strmid, index, genno)` or `(None, pos, genno)`.
pub type XRefPos = (Option<i64>, i64, i64);

/// `PDFXRef` (and `PDFXRefFallback`, which shares its state).
pub struct PDFXRef {
    /// `offsets`: object id -> position, keyed like a Python dict (the original key object is
    /// kept with the value).
    pub offsets: IndexMap<ObjKey, (Value, XRefPos)>,
    pub trailer: DictRef,
}

impl Default for PDFXRef {
    fn default() -> Self {
        Self::new()
    }
}

impl PDFXRef {
    pub fn new() -> Self {
        PDFXRef { offsets: IndexMap::new(), trailer: Value::new_dict() }
    }

    /// `self.offsets[objid] = pos` (an existing key keeps its key object and its place).
    fn set_offset(&mut self, objid: Value, pos: XRefPos) -> PyResult<()> {
        let key = obj_key(&objid)?;
        self.offsets.entry(key).and_modify(|e| e.1 = pos).or_insert((objid, pos));
        Ok(())
    }

    fn keys_repr(&self) -> String {
        let keys: Vec<String> = self.offsets.values().map(|(k, _)| k.repr()).collect();
        format!("dict_keys([{}])", keys.join(", "))
    }

    /// `PDFXRef.load(parser)`.
    pub fn load(&mut self, doc: &PDFDocument) -> PyResult<()> {
        loop {
            let (pos, line) = {
                let mut parser = doc.parser.borrow_mut();
                let mut r = parser.nextline();
                loop {
                    match r {
                        Ok((pos, line)) => {
                            let line = bytes_strip(&line).to_vec();
                            if line.is_empty() {
                                r = parser.nextline();
                                continue;
                            }
                            break (pos, line);
                        }
                        Err(e) if e.is(ExcKind::PSEOF) => {
                            return Err(PyErr::new(ExcKind::PDFNoValidXRef, "Unexpected EOF - file corrupted?"));
                        }
                        Err(e) => return Err(e),
                    }
                }
            };
            if line.starts_with(b"trailer") {
                doc.parser.borrow_mut().seek(pos)?;
                break;
            }
            let f: Vec<&[u8]> = line.split(|&c| c == b' ').collect();
            if f.len() != 2 {
                // DIVERGE(fp-repr): Python's message holds `repr(parser)`.
                let error_msg = format!("Trailer not found: <PDFParser>: line={}", bytes_repr(&line));
                return Err(PyErr::new(ExcKind::PDFNoValidXRef, error_msg));
            }
            let parsed = py_int(&Value::bytes(f[0])).and_then(|s| Ok((s, py_int(&Value::bytes(f[1]))?)));
            let (start, nobjs) = match parsed {
                Ok(v) => v,
                Err(e) if e.is(ExcKind::ValueError) => {
                    // DIVERGE(fp-repr): Python's message holds `repr(parser)`.
                    let error_msg = format!("Invalid line: <PDFParser>: line={}", bytes_repr(&line));
                    return Err(PyErr::new(ExcKind::PDFNoValidXRef, error_msg));
                }
                Err(e) => return Err(e),
            };
            let end = start.saturating_add(nobjs);
            let mut objid = start;
            while objid < end {
                let line = {
                    let mut parser = doc.parser.borrow_mut();
                    match parser.nextline() {
                        Ok((_, line)) => bytes_strip(&line).to_vec(),
                        Err(e) if e.is(ExcKind::PSEOF) => {
                            return Err(PyErr::new(ExcKind::PDFNoValidXRef, "Unexpected EOF - file corrupted?"));
                        }
                        Err(e) => return Err(e),
                    }
                };
                let f: Vec<&[u8]> = line.split(|&c| c == b' ').collect();
                if f.len() != 3 {
                    // DIVERGE(fp-repr): Python's message holds `repr(parser)`.
                    let error_msg = format!("Invalid XRef format: <PDFParser>, line={}", bytes_repr(&line));
                    return Err(PyErr::new(ExcKind::PDFNoValidXRef, error_msg));
                }
                let (pos_b, genno_b, use_b) = (f[0], f[1], f[2]);
                if use_b != b"n" {
                    objid += 1;
                    continue;
                }
                let pos_i = safe_int(&Value::bytes(pos_b))?;
                let genno_i = safe_int(&Value::bytes(genno_b))?;
                if let (Some(pos_i), Some(genno_i)) = (pos_i, genno_i) {
                    self.set_offset(Value::Int(objid), (None, pos_i, genno_i))?;
                } else {
                    warn!(
                        "Not adding object {} to xref because position {} or generation number {} cannot be parsed as an int",
                        objid,
                        bytes_repr(pos_b),
                        bytes_repr(genno_b)
                    );
                }
                objid += 1;
            }
        }
        debug!("xref objects: {}", self.keys_repr());
        self.load_trailer(doc)
    }

    /// `load_trailer(parser)`.
    pub fn load_trailer(&mut self, doc: &PDFDocument) -> PyResult<()> {
        let dic = {
            let mut parser = doc.parser.borrow_mut();
            let r: PyResult<Value> = (|| {
                let (_, kwd) = parser.nexttoken()?;
                if !is_kwd(&kwd, b"trailer") {
                    return Err(PyErr::new(ExcKind::AssertionError, kwd.py_str()));
                }
                let (_, dic) = parser.nextobject()?;
                Ok(dic)
            })();
            match r {
                Ok(dic) => dic,
                Err(e) if e.is(ExcKind::PSEOF) => {
                    let x = parser.ps.pop(1);
                    if x.is_empty() {
                        return Err(PyErr::new(ExcKind::PDFNoValidXRef, "Unexpected EOF - file corrupted"));
                    }
                    x[0].1.clone()
                }
                Err(e) => return Err(e),
            }
        };
        let d = dict_value(&dic)?;
        let items: Vec<(Rc<str>, Value)> = d.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        self.trailer.borrow_mut().extend(items);
        debug!("trailer={}", dict_repr(&self.trailer.borrow()));
        Ok(())
    }

    /// `PDFXRefFallback.load(parser)`.
    pub fn load_fallback(&mut self, doc: &PDFDocument) -> PyResult<()> {
        doc.parser.borrow_mut().seek(0)?;
        loop {
            let r = doc.parser.borrow_mut().nextline();
            let (pos, line_bytes) = match r {
                Ok(v) => v,
                Err(e) if e.is(ExcKind::PSEOF) => break,
                Err(e) => return Err(e),
            };
            if line_bytes.starts_with(b"trailer") {
                doc.parser.borrow_mut().seek(pos)?;
                self.load_trailer(doc)?;
                debug!("trailer: {}", dict_repr(&self.trailer.borrow()));
                break;
            }
            // `line_bytes.decode("latin-1")` matched against PDFOBJ_CUE.
            let Some((objid_s, genno_s)) = match_pdfobj_cue(&line_bytes) else {
                continue;
            };
            let objid = parse_digits(objid_s);
            // DIVERGE(bigint): a generation number beyond i64 saturates.
            let genno = parse_digits(genno_s).as_int().unwrap_or(i64::MAX);
            let objid_int = objid.as_int();
            self.set_offset(objid.clone(), (None, pos, genno))?;
            // expand ObjStm.
            let obj = {
                let mut parser = doc.parser.borrow_mut();
                parser.seek(pos)?;
                parser.nextobject()?.1
            };
            if let Value::Stream(s) = &obj
                && s.get("Type").is_some_and(|t| is_lit(&t, LITERAL_OBJSTM))
            {
                let stream = stream_value(&obj)?;
                let n = match stream.getitem("N") {
                    Ok(n) => n,
                    Err(e) if e.is(ExcKind::KeyError) => {
                        if settings::STRICT {
                            return Err(PyErr::new(
                                ExcKind::PDFSyntaxError,
                                format!("N is not defined: {}", stream.repr()),
                            ));
                        }
                        Value::Int(0)
                    }
                    Err(e) => return Err(e),
                };
                let data = stream.get_data()?;
                let mut parser1 = PDFStreamParser::new(&data);
                let mut objs: Vec<Value> = Vec::new();
                loop {
                    match parser1.nextobject() {
                        Ok((_, obj)) => objs.push(obj),
                        Err(e) if e.is(ExcKind::PSEOF) => break,
                        Err(e) => return Err(e),
                    }
                }
                // n = min(n, len(objs) // 2)
                let half = Value::Int((objs.len() / 2) as i64);
                let n = if py_lt(&half, &n)? { half } else { n };
                let n = range_arg(&n)?;
                // The stream's own id is an int here (it comes from the regex).
                let strmid = objid_int.unwrap_or(i64::MAX);
                for index in 0..n.max(0) {
                    let objid1 = objs[(index * 2) as usize].clone();
                    self.set_offset(objid1, (Some(strmid), index, 0))?;
                }
            }
        }
        Ok(())
    }
}

/// `int(s)` of a run of ASCII digits (a float beyond i64: `DIVERGE(bigint)`).
fn parse_digits(s: &[u8]) -> Value {
    let text = std::str::from_utf8(s).expect("digits");
    match text.parse::<i64>() {
        Ok(i) => Value::Int(i),
        Err(_) => Value::Float(text.parse::<f64>().unwrap_or(f64::INFINITY)),
    }
}

/// `\w` of a `str` regex, for the latin-1 code points of a decoded line.
fn latin1_is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || c == b'_'
        || matches!(c, 0xAA | 0xB2 | 0xB3 | 0xB5 | 0xB9 | 0xBA | 0xBC | 0xBD | 0xBE)
        || (0xC0..=0xFF).contains(&c) && c != 0xD7 && c != 0xF7
}

/// `PDFOBJ_CUE = re.compile(r"^(\d+)\s+(\d+)\s+obj\b")` on `line.decode("latin-1")`.
fn match_pdfobj_cue(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let is_space = |c: u8| py_isspace(c as u32);
    let digits = |i: usize| -> usize {
        let mut j = i;
        while j < line.len() && line[j].is_ascii_digit() {
            j += 1;
        }
        j
    };
    let spaces = |i: usize| -> usize {
        let mut j = i;
        while j < line.len() && is_space(line[j]) {
            j += 1;
        }
        j
    };
    let a_end = digits(0);
    if a_end == 0 {
        return None;
    }
    let b_start = spaces(a_end);
    if b_start == a_end {
        return None;
    }
    let b_end = digits(b_start);
    if b_end == b_start {
        return None;
    }
    let o_start = spaces(b_end);
    if o_start == b_end {
        return None;
    }
    if !line[o_start..].starts_with(b"obj") {
        return None;
    }
    let after = o_start + 3;
    if after < line.len() && latin1_is_word(line[after]) {
        return None;
    }
    Some((&line[..a_end], &line[b_start..b_end]))
}

/// `PDFXRefStream`.
pub struct PDFXRefStream {
    pub data: Option<Rc<[u8]>>,
    pub entlen: Option<Value>,
    pub fl1: Option<Value>,
    pub fl2: Option<Value>,
    pub fl3: Option<Value>,
    pub ranges: Vec<(Value, Value)>,
    pub trailer: DictRef,
}

impl Default for PDFXRefStream {
    fn default() -> Self {
        Self::new()
    }
}

impl PDFXRefStream {
    pub fn new() -> Self {
        PDFXRefStream {
            data: None,
            entlen: None,
            fl1: None,
            fl2: None,
            fl3: None,
            ranges: Vec::new(),
            trailer: Value::new_dict(),
        }
    }

    pub fn repr(&self) -> String {
        let r: Vec<String> = self.ranges.iter().map(|(a, b)| format!("({}, {})", a.repr(), b.repr())).collect();
        format!("<PDFXRefStream: ranges=[{}]>", r.join(", "))
    }

    /// `PDFXRefStream.load(parser)`.
    pub fn load(&mut self, doc: &PDFDocument) -> PyResult<()> {
        let stream = {
            let mut parser = doc.parser.borrow_mut();
            parser.nexttoken()?; // ignored
            parser.nexttoken()?; // ignored
            parser.nexttoken()?;
            parser.nextobject()?.1
        };
        let stream = match &stream {
            Value::Stream(s) if s.get("Type").is_some_and(|t| is_lit(&t, LITERAL_XREF)) => s.clone(),
            _ => return Err(PyErr::new(ExcKind::PDFNoValidXRef, "Invalid PDF stream spec.")),
        };
        let size = stream.getitem("Size")?;
        let index_array = stream.get("Index").unwrap_or_else(|| Value::tuple(vec![Value::Int(0), size]));
        if !py_len(&index_array)?.is_multiple_of(2) {
            return Err(PyErr::new(ExcKind::PDFSyntaxError, "Invalid index number"));
        }
        for pair in choplist(2, py_iter_items(&index_array)?) {
            self.ranges.push((pair[0].clone(), pair[1].clone()));
        }
        let w = stream.getitem("W")?;
        let items = match py_iter_items(&w) {
            Ok(items) => items,
            Err(e) if e.is(ExcKind::TypeError) => {
                return Err(PyErr::new(
                    ExcKind::TypeError,
                    format!("cannot unpack non-iterable {} object", w.type_name()),
                ));
            }
            Err(e) => return Err(e),
        };
        if items.len() != 3 {
            let msg = if items.len() < 3 {
                format!("not enough values to unpack (expected 3, got {})", items.len())
            } else {
                "too many values to unpack (expected 3)".to_string()
            };
            return Err(PyErr::new(ExcKind::ValueError, msg));
        }
        self.fl1 = Some(items[0].clone());
        self.fl2 = Some(items[1].clone());
        self.fl3 = Some(items[2].clone());
        self.data = Some(stream.get_data()?);
        self.entlen = Some(py_add(&py_add(&items[0], &items[1])?, &items[2])?);
        self.trailer = stream.attrs.clone();
        debug!("xref stream: {}", self.repr());
        Ok(())
    }

    /// The `(f1, f2, f3)` fields of entry `index`.
    fn fields(&self, index: &Value) -> PyResult<(i64, i64, i64)> {
        let entlen = self.entlen.as_ref().expect("loaded");
        let data = self.data.as_ref().expect("loaded");
        let (fl1, fl2) = (self.fl1.as_ref().unwrap(), self.fl2.as_ref().unwrap());
        let offset = py_mul(entlen, index)?;
        let end = py_add(&offset, entlen)?;
        let ent = py_slice_v(data, Some(&offset), Some(&end))?;
        let f1 = nunpack(py_slice_v(ent, None, Some(fl1))?, 1);
        let f12 = py_add(fl1, fl2)?;
        let f2 = nunpack(py_slice_v(ent, Some(fl1), Some(&f12))?, 0);
        let f3 = nunpack(py_slice_v(ent, Some(&f12), None)?, 0);
        Ok((f1, f2, f3))
    }

    /// `get_pos(objid)`.
    pub fn get_pos(&self, objid: &Value) -> PyResult<XRefPos> {
        let mut index = Value::Int(0);
        let mut found = false;
        for (start, nobjs) in &self.ranges {
            if py_cmp(start, objid, "<=")? && py_cmp(objid, &py_add(start, nobjs)?, "<")? {
                index = py_add(&index, &py_sub(objid, start)?)?;
                found = true;
                break;
            } else {
                index = py_add(&index, nobjs)?;
            }
        }
        if !found {
            return Err(PyErr::new(ExcKind::PDFKeyError, objid.repr()));
        }
        let (f1, f2, f3) = self.fields(&index)?;
        if f1 == 1 {
            Ok((None, f2, f3))
        } else if f1 == 2 {
            Ok((Some(f2), f3, 0))
        } else {
            // this is a free object
            Err(PyErr::new(ExcKind::PDFKeyError, objid.repr()))
        }
    }
}

/// `PDFBaseXRef` and its subclasses.
pub enum PDFBaseXRef {
    XRef(PDFXRef),
    Fallback(PDFXRef),
    Stream(PDFXRefStream),
}

impl PDFBaseXRef {
    pub fn get_trailer(&self) -> DictRef {
        match self {
            PDFBaseXRef::XRef(x) | PDFBaseXRef::Fallback(x) => x.trailer.clone(),
            PDFBaseXRef::Stream(x) => x.trailer.clone(),
        }
    }

    /// `get_objids()`: lazy (the xref stream's generator can raise part way).
    pub fn get_objids(self: &Rc<Self>) -> ObjidIter {
        let keys = match &**self {
            PDFBaseXRef::XRef(x) | PDFBaseXRef::Fallback(x) => {
                Some(x.offsets.values().map(|(k, _)| k.clone()).collect())
            }
            PDFBaseXRef::Stream(_) => None,
        };
        ObjidIter { xref: self.clone(), keys, pos: 0, range_idx: 0, i: 0, nobjs: None, done: false }
    }

    /// `get_pos(objid)`: `KeyError` (or `PDFKeyError`) when the object is not in this xref.
    pub fn get_pos(&self, objid: &Value) -> PyResult<XRefPos> {
        match self {
            PDFBaseXRef::XRef(x) | PDFBaseXRef::Fallback(x) => {
                let key = obj_key(objid)?;
                x.offsets.get(&key).map(|(_, p)| *p).ok_or_else(|| PyErr::new(ExcKind::KeyError, objid.repr()))
            }
            PDFBaseXRef::Stream(x) => x.get_pos(objid),
        }
    }

    pub fn repr(&self) -> String {
        match self {
            PDFBaseXRef::XRef(x) => format!("<PDFXRef: offsets={}>", x.keys_repr()),
            PDFBaseXRef::Fallback(x) => format!("<PDFXRefFallback: offsets={}>", x.keys_repr()),
            PDFBaseXRef::Stream(x) => x.repr(),
        }
    }
}

/// The iterator `xref.get_objids()` returns.
pub struct ObjidIter {
    xref: Rc<PDFBaseXRef>,
    keys: Option<Vec<Value>>,
    pos: usize,
    range_idx: usize,
    i: i64,
    nobjs: Option<i64>,
    done: bool,
}

impl Iterator for ObjidIter {
    type Item = PyResult<Value>;

    fn next(&mut self) -> Option<PyResult<Value>> {
        if self.done {
            return None;
        }
        if let Some(keys) = &self.keys {
            let k = keys.get(self.pos).cloned();
            self.pos += 1;
            return k.map(Ok);
        }
        let PDFBaseXRef::Stream(x) = &*self.xref else { unreachable!() };
        loop {
            if self.range_idx >= x.ranges.len() {
                self.done = true;
                return None;
            }
            let (start, nobjs) = &x.ranges[self.range_idx];
            if self.nobjs.is_none() {
                match range_arg(nobjs) {
                    Ok(n) => {
                        self.nobjs = Some(n);
                        self.i = 0;
                    }
                    Err(e) => {
                        self.done = true;
                        return Some(Err(e));
                    }
                }
            }
            if self.i >= self.nobjs.unwrap() {
                self.range_idx += 1;
                self.nobjs = None;
                continue;
            }
            let i = Value::Int(self.i);
            self.i += 1;
            let r = (|| -> PyResult<Option<Value>> {
                let entlen = x.entlen.as_ref().expect("loaded");
                let data = x.data.as_ref().expect("loaded");
                let offset = py_mul(entlen, &i)?;
                let end = py_add(&offset, entlen)?;
                let ent = py_slice_v(data, Some(&offset), Some(&end))?;
                let f1 = nunpack(py_slice_v(ent, None, x.fl1.as_ref())?, 1);
                if f1 == 1 || f1 == 2 { Ok(Some(py_add(start, &i)?)) } else { Ok(None) }
            })();
            match r {
                Ok(Some(v)) => return Some(Ok(v)),
                Ok(None) => continue,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Security handlers

const PASSWORD_PADDING: &[u8] = b"(\xbfN^Nu\x8aAd\x00NV\xff\xfa\x01\x08..\x00\xb6\xd0h>\x80/\x0c\xa9\xfedSiz";

/// Which class of the handler hierarchy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandlerClass {
    /// `PDFStandardSecurityHandler` (revisions 2 and 3).
    Standard,
    /// `PDFStandardSecurityHandlerV4` (revision 4).
    V4,
    /// `PDFStandardSecurityHandlerV5` (revisions 5 and 6).
    V5,
}

/// A crypt filter method (`self.cfm[name]`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CryptMethod {
    Rc4,
    Aes128,
    Aes256,
    Identity,
}

/// `PDFStandardSecurityHandler`, `PDFStandardSecurityHandlerV4`, `PDFStandardSecurityHandlerV5`.
pub struct PDFStandardSecurityHandler {
    pub class: HandlerClass,
    pub docid: Vec<Value>,
    pub param: DictRef,
    pub password: String,
    pub v: i64,
    pub r: i64,
    pub p: i64,
    pub o: Rc<[u8]>,
    pub u: Rc<[u8]>,
    pub length: i64,
    // V4
    pub cf: Option<DictRef>,
    pub stmf: String,
    pub strf: String,
    pub encrypt_metadata: bool,
    pub cfm: IndexMap<String, CryptMethod>,
    // V5
    pub oe: Rc<[u8]>,
    pub ue: Rc<[u8]>,
    pub o_hash: Vec<u8>,
    pub o_validation_salt: Vec<u8>,
    pub o_key_salt: Vec<u8>,
    pub u_hash: Vec<u8>,
    pub u_validation_salt: Vec<u8>,
    pub u_key_salt: Vec<u8>,
    pub key: Option<Vec<u8>>,
}

/// `str.encode("latin1")`.
fn latin1_encode(s: &str) -> PyResult<Vec<u8>> {
    let chars: Vec<char> = s.chars().collect();
    if let Some(start) = chars.iter().position(|&c| c as u32 > 0xFF) {
        let mut end = start;
        while end + 1 < chars.len() && chars[end + 1] as u32 > 0xFF {
            end += 1;
        }
        let msg = if start == end {
            let cp = chars[start] as u32;
            let esc = if cp <= 0xFFFF { format!("\\u{cp:04x}") } else { format!("\\U{cp:08x}") };
            format!("'latin-1' codec can't encode character '{esc}' in position {start}: ordinal not in range(256)")
        } else {
            format!("'latin-1' codec can't encode characters in position {start}-{end}: ordinal not in range(256)")
        };
        return Err(PyErr::new(ExcKind::UnicodeEncodeError, msg));
    }
    Ok(chars.iter().map(|&c| c as u8).collect())
}

/// `bytes((c ^ i,) for c in key)`.
fn xor_key(key: &[u8], i: u8) -> Vec<u8> {
    key.iter().map(|c| c ^ i).collect()
}

impl PDFStandardSecurityHandler {
    /// `factory(docid, param, password)`.
    pub fn new(class: HandlerClass, docid: Vec<Value>, param: DictRef, password: &str) -> PyResult<Self> {
        let empty: Rc<[u8]> = Rc::from(&b""[..]);
        let mut h = PDFStandardSecurityHandler {
            class,
            docid,
            param,
            password: password.to_string(),
            v: 0,
            r: 0,
            p: 0,
            o: empty.clone(),
            u: empty.clone(),
            length: 0,
            cf: None,
            stmf: String::new(),
            strf: String::new(),
            encrypt_metadata: true,
            cfm: IndexMap::new(),
            oe: empty.clone(),
            ue: empty,
            o_hash: vec![],
            o_validation_salt: vec![],
            o_key_salt: vec![],
            u_hash: vec![],
            u_validation_salt: vec![],
            u_key_salt: vec![],
            key: None,
        };
        h.init()?;
        Ok(h)
    }

    fn supported_revisions(&self) -> &'static [i64] {
        match self.class {
            HandlerClass::Standard => &[2, 3],
            HandlerClass::V4 => &[4],
            HandlerClass::V5 => &[5, 6],
        }
    }

    fn init(&mut self) -> PyResult<()> {
        self.init_params()?;
        if !self.supported_revisions().contains(&self.r) {
            let error_msg = format!("Unsupported revision: param={}", dict_repr(&self.param.borrow()));
            return Err(PyErr::new(ExcKind::PDFEncryptionError, error_msg));
        }
        self.init_key()
    }

    fn init_params(&mut self) -> PyResult<()> {
        // PDFStandardSecurityHandler.init_params
        self.v = int_value(&dget(&self.param, "V").unwrap_or(Value::Int(0)))?;
        self.r = int_value(&ditem(&self.param, "R")?)?;
        self.p = uint_value(&ditem(&self.param, "P")?, 32)?;
        self.o = str_value(&ditem(&self.param, "O")?)?;
        self.u = str_value(&ditem(&self.param, "U")?)?;
        self.length = int_value(&dget(&self.param, "Length").unwrap_or(Value::Int(40)))?;
        if self.class == HandlerClass::Standard {
            return Ok(());
        }
        // PDFStandardSecurityHandlerV4.init_params
        self.length = 128;
        let cf = dict_value(&dget(&self.param, "CF").unwrap_or(Value::None))?;
        self.cf = Some(cf.clone());
        self.stmf = literal_name(&ditem(&self.param, "StmF")?)?;
        self.strf = literal_name(&ditem(&self.param, "StrF")?)?;
        self.encrypt_metadata = dget(&self.param, "EncryptMetadata").unwrap_or(Value::Bool(true)).truthy();
        if self.stmf != self.strf {
            let error_msg = format!("Unsupported crypt filter: param={}", dict_repr(&self.param.borrow()));
            return Err(PyErr::new(ExcKind::PDFEncryptionError, error_msg));
        }
        self.cfm = IndexMap::new();
        let items: Vec<(Rc<str>, Value)> = cf.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (k, v) in items {
            let f = self.get_cfm(&literal_name(&py_getitem_str(&v, "CFM")?)?);
            let Some(f) = f else {
                let error_msg = format!("Unknown crypt filter method: param={}", dict_repr(&self.param.borrow()));
                return Err(PyErr::new(ExcKind::PDFEncryptionError, error_msg));
            };
            self.cfm.insert(k.to_string(), f);
        }
        self.cfm.insert("Identity".to_string(), CryptMethod::Identity);
        if !self.cfm.contains_key(&self.strf) {
            let error_msg = format!("Undefined crypt filter: param={}", dict_repr(&self.param.borrow()));
            return Err(PyErr::new(ExcKind::PDFEncryptionError, error_msg));
        }
        if self.class == HandlerClass::V4 {
            return Ok(());
        }
        // PDFStandardSecurityHandlerV5.init_params
        self.length = 256;
        self.oe = str_value(&ditem(&self.param, "OE")?)?;
        self.ue = str_value(&ditem(&self.param, "UE")?)?;
        self.o_hash = py_slice(&self.o, None, Some(32)).to_vec();
        self.o_validation_salt = py_slice(&self.o, Some(32), Some(40)).to_vec();
        self.o_key_salt = py_slice(&self.o, Some(40), None).to_vec();
        self.u_hash = py_slice(&self.u, None, Some(32)).to_vec();
        self.u_validation_salt = py_slice(&self.u, Some(32), Some(40)).to_vec();
        self.u_key_salt = py_slice(&self.u, Some(40), None).to_vec();
        Ok(())
    }

    fn get_cfm(&self, name: &str) -> Option<CryptMethod> {
        match self.class {
            HandlerClass::V5 => (name == "AESV3").then_some(CryptMethod::Aes256),
            _ => match name {
                "V2" => Some(CryptMethod::Rc4),
                "AESV2" => Some(CryptMethod::Aes128),
                _ => None,
            },
        }
    }

    fn init_key(&mut self) -> PyResult<()> {
        let password = self.password.clone();
        self.key = self.authenticate(&password)?;
        if self.key.is_none() {
            return Err(PyErr::new(ExcKind::PDFPasswordIncorrect, ""));
        }
        Ok(())
    }

    pub fn is_printable(&self) -> bool {
        self.p & 4 != 0
    }

    pub fn is_modifiable(&self) -> bool {
        self.p & 8 != 0
    }

    pub fn is_extractable(&self) -> bool {
        self.p & 16 != 0
    }

    fn docid0(&self) -> PyResult<&[u8]> {
        let first = self.docid.first().ok_or_else(|| PyErr::new(ExcKind::IndexError, "list index out of range"))?;
        hash_bytes(first)
    }

    pub fn compute_u(&self, key: &[u8]) -> PyResult<Vec<u8>> {
        if self.r == 2 {
            // Algorithm 3.4
            Ok(Arcfour::new(key)?.encrypt(PASSWORD_PADDING)) // 2
        } else {
            // Algorithm 3.5
            let mut hash = Md5::new();
            hash.update(PASSWORD_PADDING); // 2
            hash.update(self.docid0()?); // 3
            let mut result = Arcfour::new(key)?.encrypt(&hash.finalize()); // 4
            for i in 1..20u8 {
                // 5
                let k = xor_key(key, i);
                result = Arcfour::new(&k)?.encrypt(&result);
            }
            let copy = result.clone();
            result.extend(copy); // 6
            Ok(result)
        }
    }

    pub fn compute_encryption_key(&self, password: &[u8]) -> PyResult<Vec<u8>> {
        // Algorithm 3.2
        let mut password = [password, PASSWORD_PADDING].concat(); // 1
        password.truncate(32);
        let mut hash = Md5::new(); // 2
        hash.update(&password);
        hash.update(&self.o); // 3
        // See https://github.com/pdfminer/pdfminer.six/issues/186
        hash.update(pack_l(self.p)?); // 4
        hash.update(self.docid0()?); // 5
        if self.r >= 4 && !self.encrypt_metadata {
            hash.update(b"\xff\xff\xff\xff");
        }
        let mut result = hash.finalize().to_vec();
        let mut n: i64 = 5;
        if self.r >= 3 {
            n = crate::pyrt::value::int_floordiv(self.length, 8);
            for _ in 0..50 {
                result = md5_digest(&[py_slice(&result, None, Some(n))]);
            }
        }
        Ok(py_slice(&result, None, Some(n)).to_vec())
    }

    pub fn authenticate(&self, password: &str) -> PyResult<Option<Vec<u8>>> {
        if self.class == HandlerClass::V5 {
            return self.authenticate_v5(password);
        }
        let password_bytes = latin1_encode(password)?;
        let mut key = self.authenticate_user_password(&password_bytes)?;
        if key.is_none() {
            key = self.authenticate_owner_password(&password_bytes)?;
        }
        Ok(key)
    }

    pub fn authenticate_user_password(&self, password: &[u8]) -> PyResult<Option<Vec<u8>>> {
        let key = self.compute_encryption_key(password)?;
        if self.verify_encryption_key(&key)? { Ok(Some(key)) } else { Ok(None) }
    }

    pub fn verify_encryption_key(&self, key: &[u8]) -> PyResult<bool> {
        // Algorithm 3.6
        let u = self.compute_u(key)?;
        if self.r == 2 {
            return Ok(u[..] == self.u[..]);
        }
        Ok(py_slice(&u, None, Some(16)) == py_slice(&self.u, None, Some(16)))
    }

    pub fn authenticate_owner_password(&self, password: &[u8]) -> PyResult<Option<Vec<u8>>> {
        // Algorithm 3.7
        let mut password = [password, PASSWORD_PADDING].concat();
        password.truncate(32);
        let mut digest = md5_digest(&[&password]);
        if self.r >= 3 {
            for _ in 0..50 {
                digest = md5_digest(&[&digest]);
            }
        }
        let mut n: i64 = 5;
        if self.r >= 3 {
            n = crate::pyrt::value::int_floordiv(self.length, 8);
        }
        let key = py_slice(&digest, None, Some(n)).to_vec();
        let user_password = if self.r == 2 {
            Arcfour::new(&key)?.decrypt(&self.o)
        } else {
            let mut user_password = self.o.to_vec();
            for i in (0..=19u8).rev() {
                let k = xor_key(&key, i);
                user_password = Arcfour::new(&k)?.decrypt(&user_password);
            }
            user_password
        };
        self.authenticate_user_password(&user_password)
    }

    /// `decrypt(objid, genno, data, attrs=None)`.
    pub fn decrypt(&self, objid: i64, genno: i64, data: &[u8], attrs: Option<&DictRef>) -> PyResult<Vec<u8>> {
        if self.class == HandlerClass::Standard {
            return self.decrypt_rc4(objid, genno, data);
        }
        // PDFStandardSecurityHandlerV4.decrypt
        if !self.encrypt_metadata
            && let Some(attrs) = attrs
            && let Some(t) = dget(attrs, "Type")
            && !t.is_none()
            && literal_name(&t)? == "Metadata"
        {
            return Ok(data.to_vec());
        }
        let name = &self.strf;
        match self.cfm.get(name) {
            Some(CryptMethod::Rc4) => self.decrypt_rc4(objid, genno, data),
            Some(CryptMethod::Aes128) => self.decrypt_aes128(objid, genno, data),
            Some(CryptMethod::Aes256) => self.decrypt_aes256(objid, genno, data),
            Some(CryptMethod::Identity) => Ok(data.to_vec()),
            None => Err(PyErr::new(ExcKind::KeyError, str_repr(name))),
        }
    }

    fn key(&self) -> PyResult<&[u8]> {
        self.key.as_deref().ok_or_else(|| PyErr::new(ExcKind::AssertionError, ""))
    }

    pub fn decrypt_rc4(&self, objid: i64, genno: i64, data: &[u8]) -> PyResult<Vec<u8>> {
        let key = [self.key()?, &pack_l(objid)?[..3], &pack_l(genno)?[..2]].concat();
        let hash = md5_digest(&[&key]);
        let key = &hash[..key.len().min(16)];
        Ok(Arcfour::new(key)?.decrypt(data))
    }

    pub fn decrypt_aes128(&self, objid: i64, genno: i64, data: &[u8]) -> PyResult<Vec<u8>> {
        let key = [self.key()?, &pack_l(objid)?[..3], &pack_l(genno)?[..2], b"sAlT"].concat();
        let hash = md5_digest(&[&key]);
        let key = &hash[..key.len().min(16)];
        let initialization_vector = py_slice(data, None, Some(16));
        let ciphertext = py_slice(data, Some(16), None);
        let plaintext = aes_cbc_decrypt_update(key, initialization_vector, ciphertext)?;
        Ok(unpad_aes(&plaintext).to_vec())
    }

    // --- PDFStandardSecurityHandlerV5

    fn authenticate_v5(&self, password: &str) -> PyResult<Option<Vec<u8>>> {
        let password_b = self._normalize_password(password)?;
        let hash = self._password_hash(&password_b, &self.o_validation_salt, Some(&self.u))?;
        if hash == self.o_hash {
            let hash = self._password_hash(&password_b, &self.o_key_salt, Some(&self.u))?;
            return Ok(Some(aes_cbc_decrypt_update(&hash, &[0u8; 16], &self.oe)?));
        }
        let hash = self._password_hash(&password_b, &self.u_validation_salt, None)?;
        if hash == self.u_hash {
            let hash = self._password_hash(&password_b, &self.u_key_salt, None)?;
            return Ok(Some(aes_cbc_decrypt_update(&hash, &[0u8; 16], &self.ue)?));
        }
        Ok(None)
    }

    pub fn _normalize_password(&self, password: &str) -> PyResult<Vec<u8>> {
        let mut password = password.to_string();
        if self.r == 6 {
            // saslprep expects non-empty strings, apparently
            if password.is_empty() {
                return Ok(vec![]);
            }
            password = saslprep(&password, true)?;
        }
        let b = password.as_bytes();
        Ok(b[..b.len().min(127)].to_vec())
    }

    /// Compute password hash depending on revision number
    pub fn _password_hash(&self, password: &[u8], salt: &[u8], vector: Option<&[u8]>) -> PyResult<Vec<u8>> {
        if self.r == 5 {
            return Ok(Self::_r5_password(password, salt, vector));
        }
        Self::_r6_password(password, py_slice(salt, Some(0), Some(8)), vector)
    }

    /// Compute the password for revision 5
    pub fn _r5_password(password: &[u8], salt: &[u8], vector: Option<&[u8]>) -> Vec<u8> {
        let mut hash = Sha256::new();
        hash.update(password);
        hash.update(salt);
        if let Some(v) = vector {
            hash.update(v);
        }
        hash.finalize().to_vec()
    }

    /// Compute the password for revision 6
    pub fn _r6_password(password: &[u8], salt: &[u8], vector: Option<&[u8]>) -> PyResult<Vec<u8>> {
        let mut initial_hash = Sha256::new();
        initial_hash.update(password);
        initial_hash.update(salt);
        if let Some(v) = vector {
            initial_hash.update(v);
        }
        let mut k = initial_hash.finalize().to_vec();
        let mut round_no: i64 = 0;
        let mut last_byte_val: i64 = 0;
        while round_no < 64 || last_byte_val > round_no - 32 {
            let unit = [password, &k, vector.unwrap_or(b"")].concat();
            let k1 = unit.repeat(64);
            let e = aes_cbc_encrypt(&k[..16], &k[16..32], &k1)?;
            // compute the first 16 bytes of e,
            // interpreted as an unsigned integer mod 3
            k = match Self::_bytes_mod_3(&e[..16]) {
                0 => Sha256::digest(&e).to_vec(),
                1 => Sha384::digest(&e).to_vec(),
                _ => Sha512::digest(&e).to_vec(),
            };
            last_byte_val = e[e.len() - 1] as i64;
            round_no += 1;
        }
        Ok(k[..32].to_vec())
    }

    pub fn _bytes_mod_3(input_bytes: &[u8]) -> u32 {
        // 256 is 1 mod 3, so we can just sum 'em
        input_bytes.iter().map(|&b| (b % 3) as u32).sum::<u32>() % 3
    }

    pub fn decrypt_aes256(&self, _objid: i64, _genno: i64, data: &[u8]) -> PyResult<Vec<u8>> {
        let initialization_vector = py_slice(data, None, Some(16));
        let ciphertext = py_slice(data, Some(16), None);
        let plaintext = aes_cbc_decrypt_update(self.key()?, initialization_vector, ciphertext)?;
        Ok(unpad_aes(&plaintext).to_vec())
    }
}

// ---------------------------------------------------------------------------------------------
// PDFDocument

/// One item of `get_outlines()`: `(level, title, dest, action, se)`.
#[derive(Clone, Debug)]
pub struct Outline {
    pub level: i64,
    pub title: String,
    pub dest: Value,
    pub action: Value,
    pub se: Value,
}

/// `PDFDocument`: a PDF document.
///
/// Since a PDF file can be very big, normally it is not loaded at once. So PDF document has to
/// cooperate with a PDF parser in order to dynamically import the data as processing goes.
///
/// Created as `Rc<PDFDocument>` (its parser and every `PDFObjRef` hold a `Weak` back-reference);
/// caches and parser state use interior mutability.
pub struct PDFDocument {
    pub caching: bool,
    xrefs: RefCell<Vec<Rc<PDFBaseXRef>>>,
    info: RefCell<Vec<DictRef>>,
    catalog: RefCell<DictRef>,
    encryption: RefCell<Option<(Vec<Value>, DictRef)>>,
    decipher: RefCell<Option<Rc<DecipherCallable>>>,
    security_handler: RefCell<Option<Rc<PDFStandardSecurityHandler>>>,
    parser: RefCell<PDFParser>,
    /// The parser's file, for nested parses (see the module docs).
    fp: BytesIO,
    /// `parser.fallback`, readable while the parser is busy.
    parser_fallback: Cell<bool>,
    cached_objs: RefCell<HashMap<ObjKey, (Value, Value, i64)>>,
    parsed_objs: RefCell<HashMap<i64, (Rc<Vec<Value>>, Value)>>,
    is_printable: Cell<bool>,
    is_modifiable: Cell<bool>,
    is_extractable: Cell<bool>,
    self_weak: Weak<PDFDocument>,
    /// Python stack frames in use by nested `getobj` calls.
    depth: Cell<usize>,
}

impl PDFDocument {
    /// `PDFDocument(parser, password="", caching=True, fallback=True)`.
    pub fn new(parser: PDFParser, password: &str, caching: bool, fallback: bool) -> PyResult<Rc<PDFDocument>> {
        let fp = parser.ps.base.fp.clone().expect("PDFParser has a file");
        let parser_fallback = parser.fallback;
        let doc = Rc::new_cyclic(|w| PDFDocument {
            caching,
            xrefs: RefCell::new(Vec::new()),
            info: RefCell::new(Vec::new()),
            catalog: RefCell::new(Value::new_dict()),
            encryption: RefCell::new(None),
            decipher: RefCell::new(None),
            security_handler: RefCell::new(None),
            parser: RefCell::new(parser),
            fp,
            parser_fallback: Cell::new(parser_fallback),
            cached_objs: RefCell::new(HashMap::new()),
            parsed_objs: RefCell::new(HashMap::new()),
            is_printable: Cell::new(true),
            is_modifiable: Cell::new(true),
            is_extractable: Cell::new(true),
            self_weak: w.clone(),
            depth: Cell::new(0),
        });
        doc.parser.borrow_mut().set_document(Rc::downgrade(&doc));
        doc.init(password, fallback)?;
        Ok(doc)
    }

    fn init(&self, password: &str, fallback: bool) -> PyResult<()> {
        // Retrieve the information of each header that was appended
        // (maybe multiple times) at the end of the document.
        let r = self.find_xref().and_then(|pos| self.read_xref_from(pos));
        match r {
            Ok(()) => {}
            Err(e) if e.is(ExcKind::PDFNoValidXRef) => {
                if fallback {
                    self.set_parser_fallback(true);
                    let mut newxref = PDFXRef::new();
                    newxref.load_fallback(self)?;
                    self.xrefs.borrow_mut().push(Rc::new(PDFBaseXRef::Fallback(newxref)));
                }
            }
            Err(e) => return Err(e),
        }

        let xrefs = self.xrefs.borrow().clone();
        let mut found_root = false;
        for xref in xrefs {
            let trailer = xref.get_trailer();
            if trailer.borrow().is_empty() {
                continue;
            }
            // If there's an encryption info, remember it.
            if dcontains(&trailer, "Encrypt") {
                // Some documents may not have a /ID, use two empty
                // byte strings instead. Solves
                // https://github.com/pdfminer/pdfminer.six/issues/594
                let id_value = match dget(&trailer, "ID") {
                    Some(id) => list_value(&id)?,
                    None => vec![Value::bytes(b""), Value::bytes(b"")],
                };
                let encrypt = dict_value(&ditem(&trailer, "Encrypt")?)?;
                *self.encryption.borrow_mut() = Some((id_value, encrypt));
                self._initialize_password(password)?;
            }
            if let Some(info) = dget(&trailer, "Info") {
                let info = dict_value(&info)?;
                self.info.borrow_mut().push(info);
            }
            if let Some(root) = dget(&trailer, "Root") {
                // Every PDF file must have exactly one /Root dictionary.
                let catalog = dict_value(&root)?;
                *self.catalog.borrow_mut() = catalog;
                found_root = true;
                break;
            }
        }
        if !found_root {
            return Err(PyErr::new(ExcKind::PDFSyntaxError, "No /Root object! - Is this really a PDF?"));
        }
        let t = dget(&self.catalog.borrow(), "Type");
        if !t.is_some_and(|t| is_lit(&t, LITERAL_CATALOG)) && settings::STRICT {
            return Err(PyErr::new(ExcKind::PDFSyntaxError, "Catalog not found!"));
        }
        Ok(())
    }

    fn set_parser_fallback(&self, v: bool) {
        self.parser_fallback.set(v);
        self.parser.borrow_mut().fallback = v;
    }

    // --- accessors (Python attributes)

    /// `doc.xrefs`.
    pub fn xrefs(&self) -> Vec<Rc<PDFBaseXRef>> {
        self.xrefs.borrow().clone()
    }

    /// `doc.info`.
    pub fn info(&self) -> Vec<DictRef> {
        self.info.borrow().clone()
    }

    /// `doc.catalog` (the shared dict).
    pub fn catalog(&self) -> DictRef {
        self.catalog.borrow().clone()
    }

    /// `doc.encryption`: `(docid, param)`.
    pub fn encryption(&self) -> Option<(Vec<Value>, DictRef)> {
        self.encryption.borrow().clone()
    }

    /// The security handler `_initialize_password` created.
    pub fn security_handler(&self) -> Option<Rc<PDFStandardSecurityHandler>> {
        self.security_handler.borrow().clone()
    }

    pub fn is_printable(&self) -> bool {
        self.is_printable.get()
    }

    pub fn is_modifiable(&self) -> bool {
        self.is_modifiable.get()
    }

    pub fn is_extractable(&self) -> bool {
        self.is_extractable.get()
    }

    /// `doc.decipher`: the security handler's `decrypt`, or `None` for unencrypted documents.
    pub fn decipher(&self) -> Option<Rc<DecipherCallable>> {
        self.decipher.borrow().clone()
    }

    /// A `Weak` reference to this document (what `PDFObjRef`s and parsers hold).
    pub fn weak(&self) -> Weak<PDFDocument> {
        self.self_weak.clone()
    }

    /// `_initialize_password(password)`: perform the initialization with a given password.
    fn _initialize_password(&self, password: &str) -> PyResult<()> {
        let (docid, param) = self.encryption.borrow().clone().expect("encryption is set");
        if literal_name(&dget(&param, "Filter").unwrap_or(Value::None))? != "Standard" {
            return Err(PyErr::new(
                ExcKind::PDFEncryptionError,
                format!("Unknown filter: param={}", dict_repr(&param.borrow())),
            ));
        }
        let v = int_value(&dget(&param, "V").unwrap_or(Value::Int(0)))?;
        let class = match v {
            1 | 2 => HandlerClass::Standard,
            4 => HandlerClass::V4,
            5 => HandlerClass::V5,
            _ => {
                return Err(PyErr::new(
                    ExcKind::PDFEncryptionError,
                    format!("Unknown algorithm: param={}", dict_repr(&param.borrow())),
                ));
            }
        };
        let handler = Rc::new(PDFStandardSecurityHandler::new(class, docid, param, password)?);
        let h = handler.clone();
        let decipher: Rc<DecipherCallable> =
            Rc::new(move |objid, genno, data: &[u8], attrs: Option<&DictRef>| h.decrypt(objid, genno, data, attrs));
        *self.decipher.borrow_mut() = Some(decipher);
        self.is_printable.set(handler.is_printable());
        self.is_modifiable.set(handler.is_modifiable());
        self.is_extractable.set(handler.is_extractable());
        *self.security_handler.borrow_mut() = Some(handler);
        self.set_parser_fallback(false); // need to read streams with exact length
        Ok(())
    }

    fn _getobj_objstm(&self, stream: &Rc<PDFStream>, index: i64, _objid: &Value) -> PyResult<Value> {
        let sid = stream.objid.get();
        let cached = sid.and_then(|s| self.parsed_objs.borrow().get(&s).cloned());
        let (objs, n) = match cached {
            Some(x) => x,
            None => {
                let (objs, n) = self._get_objects(stream)?;
                let x = (Rc::new(objs), n);
                if self.caching {
                    let Some(s) = sid else {
                        return Err(PyErr::new(ExcKind::AssertionError, ""));
                    };
                    self.parsed_objs.borrow_mut().insert(s, x.clone());
                }
                x
            }
        };
        let i = py_add(&py_mul(&n, &Value::Int(2))?, &Value::Int(index))?;
        match list_index(&objs, &i) {
            Ok(obj) => Ok(obj.clone()),
            Err(e) if e.is(ExcKind::IndexError) => {
                Err(PyErr::new(ExcKind::PDFSyntaxError, format!("index too big: {index}")))
            }
            Err(e) => Err(e),
        }
    }

    fn _get_objects(&self, stream: &Rc<PDFStream>) -> PyResult<(Vec<Value>, Value)> {
        if !stream.get("Type").is_some_and(|t| is_lit(&t, LITERAL_OBJSTM)) && settings::STRICT {
            return Err(PyErr::new(ExcKind::PDFSyntaxError, format!("Not a stream object: {}", stream.repr())));
        }
        let n = match stream.getitem("N") {
            Ok(n) => n,
            Err(e) if e.is(ExcKind::KeyError) => {
                if settings::STRICT {
                    return Err(PyErr::new(ExcKind::PDFSyntaxError, format!("N is not defined: {}", stream.repr())));
                }
                Value::Int(0)
            }
            Err(e) => return Err(e),
        };
        let mut parser = PDFStreamParser::new(&stream.get_data()?);
        parser.set_document(self.self_weak.clone());
        let mut objs: Vec<Value> = Vec::new();
        loop {
            match parser.nextobject() {
                Ok((_, obj)) => objs.push(obj),
                Err(e) if e.is(ExcKind::PSEOF) => break,
                Err(e) => return Err(e),
            }
        }
        Ok((objs, n))
    }

    /// A parser positioned by its caller: the document's own, or a fresh one over the same file
    /// when the document's parser is busy (see the module docs).
    fn with_parser<R>(&self, f: impl FnOnce(&mut PDFParser) -> PyResult<R>) -> PyResult<R> {
        match self.parser.try_borrow_mut() {
            Ok(mut p) => f(&mut p),
            Err(_) => {
                let mut p = PDFParser::new(self.fp.clone());
                p.set_document(self.self_weak.clone());
                p.fallback = self.parser_fallback.get();
                f(&mut p)
            }
        }
    }

    fn _getobj_parse(&self, pos: i64, objid: &Value) -> PyResult<Value> {
        self.with_parser(|parser| {
            parser.seek(pos)?;
            let (_, mut objid1) = parser.nexttoken()?; // objid
            let (_, _genno) = parser.nexttoken()?; // genno
            let (_, mut kwd) = parser.nexttoken()?;
            // hack around malformed pdf files
            // copied from https://github.com/jaepil/pdfminer3k/blob/master/
            // pdfminer/pdfparser.py#L399
            // to solve https://github.com/pdfminer/pdfminer.six/issues/56
            // assert objid1 == objid, str((objid1, objid))
            if !py_eq(&objid1, objid) {
                let mut x: Vec<Value> = Vec::new();
                while !is_kwd(&kwd, b"obj") {
                    let (_, k) = parser.nexttoken()?;
                    kwd = k.clone();
                    x.push(k);
                }
                if x.len() >= 2 {
                    objid1 = x[x.len() - 2].clone();
                }
            }
            // #### end hack around malformed pdf files
            if !py_eq(&objid1, objid) {
                return Err(PyErr::new(
                    ExcKind::PDFSyntaxError,
                    format!("objid mismatch: {}={}", objid1.repr(), objid.repr()),
                ));
            }
            if !is_kwd(&kwd, b"obj") {
                return Err(PyErr::new(ExcKind::PDFSyntaxError, format!("Invalid object spec: offset={pos}")));
            }
            let (_, obj) = parser.nextobject()?;
            Ok(obj)
        })
    }

    /// `getobj(objid)`: get object from PDF.
    ///
    /// Raises `PDFException` if the document is not initialized and `PDFObjectNotFound` if
    /// `objid` does not exist in the PDF.
    pub fn getobj(&self, objid: i64) -> PyResult<Value> {
        self.getobj_value(&Value::Int(objid))
    }

    /// `getobj(objid)` for any object id Python could pass (see the module docs).
    pub fn getobj_value(&self, objid: &Value) -> PyResult<Value> {
        if self.xrefs.borrow().is_empty() {
            return Err(PyErr::new(ExcKind::PDFException, "PDFDocument is not initialized"));
        }
        debug!("getobj: objid={}", objid.repr());
        let depth = self.depth.get();
        if depth >= RECURSION_LIMIT {
            return Err(recursion_error());
        }
        self.depth.set(depth + 1);
        let r = self.getobj_inner(objid);
        self.depth.set(depth);
        r
    }

    fn getobj_inner(&self, objid: &Value) -> PyResult<Value> {
        let key = obj_key(objid)?;
        if let Some((_, obj, _genno)) = self.cached_objs.borrow().get(&key) {
            return Ok(obj.clone());
        }
        let xrefs = self.xrefs.borrow().clone();
        let mut found: Option<(Value, i64)> = None;
        for xref in &xrefs {
            let (strmid, index, genno) = match xref.get_pos(objid) {
                Ok(p) => p,
                Err(e) if e.is(ExcKind::KeyError) => continue,
                Err(e) => return Err(e),
            };
            let r = (|| -> PyResult<Value> {
                let obj = if let Some(strmid) = strmid {
                    let stream = stream_value(&self.getobj(strmid)?)?;
                    self._getobj_objstm(&stream, index, objid)?
                } else {
                    // Python's frames for _getobj_parse -> nextobject -> do_keyword -> int_value
                    // -> resolve1 -> resolve, before a nested getobj.
                    let depth = self.depth.get();
                    self.depth.set(depth + 6);
                    let obj = self._getobj_parse(index, objid);
                    self.depth.set(depth);
                    let mut obj = obj?;
                    if let Some(decipher) = self.decipher() {
                        // `struct.pack("<L", objid)`: object ids reaching here are ints.
                        let id = objid
                            .as_int()
                            .ok_or_else(|| PyErr::new(ExcKind::StructError, "required argument is not an integer"))?;
                        obj = decipher_all(&*decipher, id, genno, &obj)?;
                    }
                    obj
                };
                // DIVERGE(nonint-objid): `PDFStream.objid` is an `i64`; a stream reached through a
                // non-int object id (which only a broken object stream header produces, and which
                // fails in `get_pos` before a stream is parsed) would keep `objid = None`.
                if let Value::Stream(s) = &obj
                    && let Some(id) = objid.as_int()
                {
                    s.set_objid(id, genno);
                }
                Ok(obj)
            })();
            match r {
                Ok(obj) => {
                    found = Some((obj, genno));
                    break;
                }
                Err(e) if e.is_any(&[ExcKind::PSEOF, ExcKind::PDFSyntaxError]) => continue,
                Err(e) => return Err(e),
            }
        }
        let Some((obj, genno)) = found else {
            return Err(PyErr::new(ExcKind::PDFObjectNotFound, objid.py_str()));
        };
        debug!("register: objid={}: {}", objid.repr(), obj.repr());
        if self.caching {
            self.cached_objs.borrow_mut().insert(key, (objid.clone(), obj.clone(), genno));
        }
        Ok(obj)
    }

    /// `get_outlines()`: `PDFNoOutlines` right away when the catalog has none; the outline items
    /// come lazily, as from Python's generator.
    pub fn get_outlines(&self) -> PyResult<OutlineIter> {
        let Some(outlines) = dget(&self.catalog(), "Outlines") else {
            return Err(PyErr::new(ExcKind::PDFNoOutlines, ""));
        };
        Ok(OutlineIter { stack: Vec::new(), pending: Some((outlines, 0)), done: false })
    }

    /// `get_page_labels()`: generate page label strings for the PDF document.
    ///
    /// If the document includes page labels, generates strings, one per page. If not, raises
    /// `PDFNoPageLabels`. The resulting iteration is unbounded.
    pub fn get_page_labels(&self) -> PyResult<PageLabelsIter> {
        let r = ditem(&self.catalog(), "PageLabels").and_then(|v| PageLabels::new(&v));
        match r {
            Ok(page_labels) => Ok(page_labels.labels()),
            Err(e) if e.is_any(&[ExcKind::PDFTypeError, ExcKind::KeyError]) => {
                Err(PyErr::new(ExcKind::PDFNoPageLabels, ""))
            }
            Err(e) => Err(e),
        }
    }

    /// `lookup_name(cat, key)` (`key` is a `str` or `bytes`).
    pub fn lookup_name(&self, cat: &str, key: &Value) -> PyResult<Value> {
        let not_found = || PyErr::new(ExcKind::PDFKeyError, format!("({}, {})", str_repr(cat), key.repr()));
        let names = match ditem(&self.catalog(), "Names").and_then(|n| dict_value(&n)) {
            Ok(n) => n,
            Err(e) if e.is_any(&[ExcKind::PDFTypeError, ExcKind::KeyError]) => return Err(not_found()),
            Err(e) => return Err(e),
        };
        // may raise KeyError
        let d0 = dict_value(&ditem(&names, cat)?)?;

        fn lookup(d: &DictRef, key: &Value, not_found: &dyn Fn() -> PyErr, depth: usize) -> PyResult<Value> {
            if depth > RECURSION_LIMIT {
                return Err(recursion_error());
            }
            if let Some(limits) = dget(d, "Limits") {
                let limits = list_value(&limits)?;
                if limits.len() != 2 {
                    let msg = if limits.len() < 2 {
                        format!("not enough values to unpack (expected 2, got {})", limits.len())
                    } else {
                        "too many values to unpack (expected 2)".to_string()
                    };
                    return Err(PyErr::new(ExcKind::ValueError, msg));
                }
                let (k1, k2) = (&limits[0], &limits[1]);
                if py_cmp(key, k1, "<")? || py_cmp(k2, key, "<")? {
                    return Ok(Value::None);
                }
            }
            if let Some(names) = dget(d, "Names") {
                let objs = list_value(&names)?;
                // dict(choplist(2, objs))[key]
                let mut found: Option<Value> = None;
                let wanted = obj_key(key)?;
                for pair in choplist(2, objs) {
                    if obj_key(&pair[0])? == wanted {
                        found = Some(pair[1].clone());
                    }
                }
                return found.ok_or_else(|| PyErr::new(ExcKind::KeyError, key.repr()));
            }
            if let Some(kids) = dget(d, "Kids") {
                for c in list_value(&kids)? {
                    let v = lookup(&dict_value(&c)?, key, not_found, depth + 1)?;
                    if v.truthy() {
                        return Ok(v);
                    }
                }
            }
            Err(not_found())
        }

        lookup(&d0, key, &not_found, 0)
    }

    /// `get_dest(name)` (`name` is a `str` or `bytes`).
    pub fn get_dest(&self, name: &Value) -> PyResult<Value> {
        match self.lookup_name("Dests", name) {
            // PDF-1.2 or later
            Ok(obj) => Ok(obj),
            Err(e) if e.is(ExcKind::KeyError) => {
                // PDF-1.1 or prior
                let catalog = self.catalog();
                let not_found = || PyErr::new(ExcKind::PDFDestinationNotFound, name.py_str());
                let Some(dests) = dget(&catalog, "Dests") else {
                    return Err(not_found());
                };
                let d0 = dict_value(&dests)?;
                let v = match name {
                    Value::Str(s) => dget(&d0, s),
                    _ => None,
                };
                v.ok_or_else(not_found)
            }
            Err(e) => Err(e),
        }
    }

    /// `find_xref(parser)`: internal function used to locate the first XRef.
    pub fn find_xref(&self) -> PyResult<i64> {
        // search the last xref table by scanning the file backwards.
        let lines = {
            let parser = self.parser.borrow();
            RevReadLines::new(parser.ps.base.fp.as_ref().expect("parser has a file"))
        };
        let mut prev: Vec<u8> = Vec::new();
        for line in lines {
            let line = bytes_strip(&line);
            debug!("find_xref: {}", bytes_repr(line));

            if line == b"startxref" {
                debug!("xref found: pos={}", bytes_repr(&prev));

                if !bytes_isdigit(&prev) {
                    return Err(PyErr::new(
                        ExcKind::PDFNoValidXRef,
                        format!("Invalid xref position, no digit: {}", bytes_repr(&prev)),
                    ));
                }

                // (Digits only: never negative.)
                let text = std::str::from_utf8(&prev).expect("digits");
                let start = text.parse::<i64>().ok().filter(|&s| s < (1i64 << 31));
                // The xref start needs to fit in a C ssize_t to be a proper file offset
                let Some(start) = start else {
                    let digits = text.trim_start_matches('0');
                    let digits = if digits.is_empty() { "0" } else { digits };
                    return Err(PyErr::new(
                        ExcKind::PDFNoValidXRef,
                        format!("Invalid xref position, too large: {digits}"),
                    ));
                };
                return Ok(start);
            }

            if !line.is_empty() {
                prev = line.to_vec();
            }
        }
        Err(PyErr::new(ExcKind::PDFNoValidXRef, "Unexpected EOF"))
    }

    /// `read_xref_from(parser, start, xrefs)`: reads XRefs from the given location, appending
    /// them to `self.xrefs` as Python does.
    ///
    /// Python recurses into `/XRefStm` and then `/Prev`; the port walks the same order with an
    /// explicit stack (a `/Prev` cycle ends in `RecursionError`, as in Python).
    pub fn read_xref_from(&self, start: i64) -> PyResult<()> {
        // (trailer, stage): stage 0 = `/XRefStm` next, 1 = `/Prev` next, 2 = done.
        let mut stack: Vec<(DictRef, u8)> = vec![(self.read_one_xref(start)?, 0)];
        while let Some(top) = stack.last_mut() {
            let key = match top.1 {
                0 => "XRefStm",
                1 => "Prev", // find previous xref
                _ => {
                    stack.pop();
                    continue;
                }
            };
            top.1 += 1;
            if let Some(v) = dget(&top.0, key) {
                let pos = int_value(&v)?;
                if stack.len() > RECURSION_LIMIT {
                    return Err(recursion_error());
                }
                let trailer = self.read_one_xref(pos)?;
                stack.push((trailer, 0));
            }
        }
        Ok(())
    }

    /// The body of `read_xref_from` before its recursive calls: loads one xref, appends it and
    /// returns its trailer.
    fn read_one_xref(&self, start: i64) -> PyResult<DictRef> {
        let (pos, token) = {
            let mut parser = self.parser.borrow_mut();
            parser.seek(start)?;
            parser.ps.reset();
            match parser.nexttoken() {
                Ok(t) => t,
                Err(e) if e.is(ExcKind::PSEOF) => {
                    return Err(PyErr::new(ExcKind::PDFNoValidXRef, "Unexpected EOF"));
                }
                Err(e) => return Err(e),
            }
        };
        debug!("read_xref_from: start={}, token={}", start, token.repr());
        let xref = if token.is_int() {
            // XRefStream: PDF-1.5
            {
                let mut parser = self.parser.borrow_mut();
                parser.seek(pos)?;
                parser.ps.reset();
            }
            let mut xref = PDFXRefStream::new();
            xref.load(self)?;
            PDFBaseXRef::Stream(xref)
        } else {
            if is_kwd(&token, b"xref") {
                self.parser.borrow_mut().nextline()?;
            }
            let mut xref = PDFXRef::new();
            xref.load(self)?;
            PDFBaseXRef::XRef(xref)
        };
        let xref = Rc::new(xref);
        self.xrefs.borrow_mut().push(xref.clone());
        let trailer = xref.get_trailer();
        debug!("trailer: {}", dict_repr(&trailer.borrow()));
        Ok(trailer)
    }
}

/// The generator `get_outlines()` returns (`search(entry, level)`, recursive in Python).
pub struct OutlineIter {
    /// `(entry, level, stage)`: stage 0 = the item was handled, 1 = `First` was handled.
    stack: Vec<(DictRef, i64, u8)>,
    pending: Option<(Value, i64)>,
    done: bool,
}

impl OutlineIter {
    fn fail(&mut self, e: PyErr) -> Option<PyResult<Outline>> {
        self.done = true;
        Some(Err(e))
    }
}

impl Iterator for OutlineIter {
    type Item = PyResult<Outline>;

    fn next(&mut self) -> Option<PyResult<Outline>> {
        loop {
            if self.done {
                return None;
            }
            if let Some((entry, level)) = self.pending.take() {
                if self.stack.len() >= RECURSION_LIMIT {
                    return self.fail(recursion_error());
                }
                let r = (|| -> PyResult<(DictRef, Option<Outline>)> {
                    let entry = dict_value(&entry)?;
                    let mut item = None;
                    if dcontains(&entry, "Title") && (dcontains(&entry, "A") || dcontains(&entry, "Dest")) {
                        let title = decode_text(&str_value(&ditem(&entry, "Title")?)?);
                        let dest = dget(&entry, "Dest").unwrap_or(Value::None);
                        let action = dget(&entry, "A").unwrap_or(Value::None);
                        let se = dget(&entry, "SE").unwrap_or(Value::None);
                        item = Some(Outline { level, title, dest, action, se });
                    }
                    Ok((entry, item))
                })();
                match r {
                    Ok((entry, item)) => {
                        self.stack.push((entry, level, 0));
                        if let Some(item) = item {
                            return Some(Ok(item));
                        }
                        continue;
                    }
                    Err(e) => return self.fail(e),
                }
            }
            let Some(top) = self.stack.last_mut() else {
                self.done = true;
                return None;
            };
            let (entry, level) = (top.0.clone(), top.1);
            match top.2 {
                0 => {
                    top.2 = 1;
                    if dcontains(&entry, "First") && dcontains(&entry, "Last") {
                        self.pending = Some((dget(&entry, "First").unwrap(), level + 1));
                    }
                }
                1 => {
                    top.2 = 2;
                    if let Some(next) = dget(&entry, "Next") {
                        self.pending = Some((next, level));
                    }
                }
                _ => {
                    self.stack.pop();
                }
            }
        }
    }
}

/// `PageLabels` from the document catalog. See Section 8.3.1 in the PDF Reference.
pub struct PageLabels {
    pub tree: NumberTree,
}

impl PageLabels {
    pub fn new(obj: &Value) -> PyResult<Self> {
        Ok(PageLabels { tree: NumberTree::new(obj)? })
    }

    /// The `labels` property (a generator).
    pub fn labels(self) -> PageLabelsIter {
        PageLabelsIter { tree: self.tree, ranges: None, next: 0, current: None, done: false }
    }

    /// `_format_page_label(value, style)`: format page label value in a specific style.
    pub fn _format_page_label(value: i64, style: &Value) -> PyResult<String> {
        Ok(if style.is_none() {
            String::new()
        } else if is_lit(style, "D") {
            // Decimal arabic numerals
            value.to_string()
        } else if is_lit(style, "R") {
            // Uppercase roman numerals
            format_int_roman(value)?.to_uppercase()
        } else if is_lit(style, "r") {
            // Lowercase roman numerals
            format_int_roman(value)?
        } else if is_lit(style, "A") {
            // Uppercase letters A-Z, AA-ZZ...
            format_int_alpha(value)?.to_uppercase()
        } else if is_lit(style, "a") {
            // Lowercase letters a-z, aa-zz...
            format_int_alpha(value)?
        } else {
            warn!("Unknown page label style: {}", style.repr());
            String::new()
        })
    }
}

/// The generator `PageLabels.labels` returns (unbounded).
pub struct PageLabelsIter {
    tree: NumberTree,
    ranges: Option<Vec<(i64, Value)>>,
    /// Index of the next range to start.
    next: usize,
    /// `(prefix, style, next value, end)` of the current range (`None` end: `itertools.count`).
    current: Option<(String, Value, i64, Option<i64>)>,
    done: bool,
}

impl PageLabelsIter {
    fn step(&mut self) -> PyResult<Option<String>> {
        if self.ranges.is_none() {
            let mut ranges = self.tree.values()?;
            // The tree must begin with page index 0
            if ranges.is_empty() || ranges[0].0 != 0 {
                if settings::STRICT {
                    return Err(PyErr::new(ExcKind::PDFSyntaxError, "PageLabels is missing page index 0"));
                }
                // Try to cope, by assuming empty labels for the initial pages
                ranges.insert(0, (0, Value::Dict(Value::new_dict())));
            }
            self.ranges = Some(ranges);
        }
        loop {
            if let Some((prefix, style, value, end)) = &mut self.current {
                if end.is_none_or(|e| *value < e) {
                    let v = *value;
                    *value += 1;
                    let label = PageLabels::_format_page_label(v, style)?;
                    return Ok(Some(format!("{prefix}{label}")));
                }
                self.current = None;
            }
            let ranges = self.ranges.as_ref().unwrap();
            if self.next >= ranges.len() {
                return Ok(None);
            }
            let (start, label_dict_unchecked) = ranges[self.next].clone();
            self.next += 1;
            let next = self.next;
            let label_dict = dict_value(&label_dict_unchecked)?;
            let style = dget(&label_dict, "S").unwrap_or(Value::None);
            let prefix = decode_text(&str_value(&dget(&label_dict, "P").unwrap_or(Value::bytes(b"")))?);
            let first_value = int_value(&dget(&label_dict, "St").unwrap_or(Value::Int(1)))?;
            let end = if next == ranges.len() {
                // This is the last specified range. It continues until the end
                // of the document.
                None
            } else {
                let (end, _) = &ranges[next];
                let range_length = end - start;
                Some(first_value.saturating_add(range_length))
            };
            self.current = Some((prefix, style, first_value, end));
        }
    }
}

impl Iterator for PageLabelsIter {
    type Item = PyResult<String>;

    fn next(&mut self) -> Option<PyResult<String>> {
        if self.done {
            return None;
        }
        match self.step() {
            Ok(Some(s)) => Some(Ok(s)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
