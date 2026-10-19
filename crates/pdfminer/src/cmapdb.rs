//! Port of `pdfminer/cmapdb.py`.
//!
//! Adobe character mapping (CMap) support.
//!
//! CMaps provide the mapping between character codes and Unicode
//! code-points to character ids (CIDs).
//!
//! More information is available on:
//!
//!   https://github.com/adobe-type-tools/cmap-resources
//!
//! The class hierarchy (`CMapBase` <- `CMap` <- `FileCMap`/`PyCMap`, `CMapBase` <-
//! `IdentityCMap` <- `IdentityCMapByte`, `CMapBase` <- `UnicodeMap` <- `IdentityUnicodeMap`/
//! `FileUnicodeMap`/`PyUnicodeMap`) is one struct, [`CMapBase`], whose [`CMapClass`] selects the
//! overridden methods. The cmap data (`pdfminer/cmap/*.json.gz`) is embedded at build time from
//! the submodule (see `build.rs`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::rc::Rc;

use indexmap::IndexMap;
use log::{debug, warn};

use crate::encodingdb::{NameArg, name2unicode_arg};
use crate::psparser::{PSKeyword, PSStackParser, PSStackParserImpl, literal_name};
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::{pystr_repr, str_repr};
use crate::pyrt::io::BytesIO;
use crate::pyrt::pystr::PyStr;
use crate::pyrt::value::{Value, py_eq, py_int};
use crate::utils::{choplist, nunpack};

mod data {
    include!(concat!(env!("OUT_DIR"), "/cmap_files.rs"));
}

/// `CMapError`.
///
/// `CMapDB.CMapNotFound(name)`.
pub fn cmap_not_found(name: &str) -> PyErr {
    PyErr::new(ExcKind::CMapNotFound, str_repr(name))
}

/// `isinstance(e, CMapDB.CMapNotFound)`.
pub fn is_cmap_not_found(e: &PyErr) -> bool {
    e.is(ExcKind::CMapNotFound)
}

/// A key of a `code2cid` dict: an int, or a string key `int()` could not convert.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Code2CidKey {
    Int(i64),
    Str(Rc<str>),
}

/// A value of a `code2cid` dict: a cid, a nested dict, or (never in the pinned data) another
/// JSON value, kept by its Python type name.
#[derive(Clone, Debug)]
pub enum Code2CidValue {
    Cid(i64),
    Dict(Rc<Code2Cid>),
    Other(&'static str),
}

/// `dict[int, object]` of `CMap.code2cid` (a tree of dicts). Keys 0..=255, the only ones a byte
/// can look up, are kept in a table; other keys in a map.
#[derive(Clone, Debug, Default)]
pub struct Code2Cid {
    bytes: Vec<Option<Code2CidValue>>,
    other: IndexMap<Code2CidKey, Code2CidValue>,
}

impl Code2Cid {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.other.is_empty() && self.bytes.iter().all(|v| v.is_none())
    }

    pub fn len(&self) -> usize {
        self.other.len() + self.bytes.iter().filter(|v| v.is_some()).count()
    }

    /// `d.get(k)` for an int key.
    #[inline]
    pub fn get_int(&self, k: i64) -> Option<&Code2CidValue> {
        if (0..256).contains(&k) {
            self.bytes.get(k as usize).and_then(|v| v.as_ref())
        } else {
            self.other.get(&Code2CidKey::Int(k))
        }
    }

    pub fn get(&self, k: &Code2CidKey) -> Option<&Code2CidValue> {
        match k {
            Code2CidKey::Int(i) => self.get_int(*i),
            _ => self.other.get(k),
        }
    }

    /// `d[k] = v`.
    pub fn insert(&mut self, k: Code2CidKey, v: Code2CidValue) {
        match k {
            Code2CidKey::Int(i) if (0..256).contains(&i) => {
                if self.bytes.is_empty() {
                    self.bytes = vec![None; 256];
                }
                self.bytes[i as usize] = Some(v);
            }
            k => {
                self.other.insert(k, v);
            }
        }
    }

    /// The items, ints first (sorted), then the other keys.
    pub fn items(&self) -> Vec<(Code2CidKey, &Code2CidValue)> {
        let mut out: Vec<(Code2CidKey, &Code2CidValue)> = Vec::new();
        for (i, v) in self.bytes.iter().enumerate() {
            if let Some(v) = v {
                out.push((Code2CidKey::Int(i as i64), v));
            }
        }
        for (k, v) in &self.other {
            out.push((k.clone(), v));
        }
        out
    }
}

/// The classes of the CMap hierarchy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CMapClass {
    CMapBase,
    CMap,
    IdentityCMap,
    IdentityCMapByte,
    UnicodeMap,
    IdentityUnicodeMap,
    FileCMap,
    FileUnicodeMap,
    PyCMap,
    PyUnicodeMap,
}

impl CMapClass {
    pub fn name(self) -> &'static str {
        match self {
            CMapClass::CMapBase => "CMapBase",
            CMapClass::CMap => "CMap",
            CMapClass::IdentityCMap => "IdentityCMap",
            CMapClass::IdentityCMapByte => "IdentityCMapByte",
            CMapClass::UnicodeMap => "UnicodeMap",
            CMapClass::IdentityUnicodeMap => "IdentityUnicodeMap",
            CMapClass::FileCMap => "FileCMap",
            CMapClass::FileUnicodeMap => "FileUnicodeMap",
            CMapClass::PyCMap => "PyCMap",
            CMapClass::PyUnicodeMap => "PyUnicodeMap",
        }
    }
}

/// A unicode map's `cid2unichr` dict (shared like Python shares `module.CID2UNICHR_H`).
pub type Cid2Unichr = HashMap<i64, PyStr>;

/// `CMapBase` and its subclasses.
#[derive(Clone, Debug)]
pub struct CMapBase {
    pub class: CMapClass,
    pub attrs: IndexMap<String, Value>,
    /// `CMap.code2cid` (CMap classes only).
    pub code2cid: Rc<Code2Cid>,
    /// `UnicodeMap.cid2unichr` (UnicodeMap classes only).
    pub cid2unichr: Rc<Cid2Unichr>,
}

impl CMapBase {
    /// `<Class>(**kwargs)`.
    pub fn new(class: CMapClass, attrs: &[(&str, Value)]) -> Self {
        CMapBase {
            class,
            attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            code2cid: Rc::new(Code2Cid::new()),
            cid2unichr: Rc::new(HashMap::new()),
        }
    }

    /// `CMap()`.
    pub fn new_cmap() -> Self {
        Self::new(CMapClass::CMap, &[])
    }

    /// `FileUnicodeMap()`.
    pub fn new_file_unicode_map() -> Self {
        Self::new(CMapClass::FileUnicodeMap, &[])
    }

    /// `isinstance(x, CMap)`.
    pub fn is_cmap(&self) -> bool {
        matches!(self.class, CMapClass::CMap | CMapClass::FileCMap | CMapClass::PyCMap)
    }

    /// `isinstance(x, IdentityCMap)`.
    pub fn is_identity_cmap(&self) -> bool {
        matches!(self.class, CMapClass::IdentityCMap | CMapClass::IdentityCMapByte)
    }

    /// `isinstance(x, UnicodeMap)`.
    pub fn is_unicode_map(&self) -> bool {
        matches!(
            self.class,
            CMapClass::UnicodeMap | CMapClass::IdentityUnicodeMap | CMapClass::FileUnicodeMap | CMapClass::PyUnicodeMap
        )
    }

    pub fn is_vertical(&self) -> bool {
        match self.attrs.get("WMode") {
            Some(v) => !py_eq(v, &Value::Int(0)),
            None => false,
        }
    }

    pub fn set_attr(&mut self, k: String, v: Value) {
        self.attrs.insert(k, v);
    }

    /// `repr(cmap)`.
    pub fn repr(&self) -> String {
        let name = || self.attrs.get("CMapName").map_or("None".to_string(), |v| v.py_str());
        if self.is_cmap() {
            format!("<CMap: {}>", name())
        } else if self.is_unicode_map() {
            format!("<UnicodeMap: {}>", name())
        } else {
            // object.__repr__ (without the address)
            format!("<pdfminer.cmapdb.{} object>", self.class.name())
        }
    }

    /// `add_code2cid(code, cid)` (`FileCMap` stores it; the other classes ignore it).
    pub fn add_code2cid(&mut self, code: &str, cid: i64) -> PyResult<()> {
        if self.class != CMapClass::FileCMap {
            return Ok(());
        }
        let chars: Vec<char> = code.chars().collect();
        let Some((last, init)) = chars.split_last() else {
            return Err(PyErr::new(ExcKind::IndexError, "string index out of range"));
        };
        fn add(d: &mut Code2Cid, path: &[char], last: char, cid: i64) -> PyResult<()> {
            match path.split_first() {
                None => {
                    d.insert(Code2CidKey::Int(last as i64), Code2CidValue::Cid(cid));
                    Ok(())
                }
                Some((&c, rest)) => {
                    let ci = Code2CidKey::Int(c as i64);
                    match d.get(&ci).cloned() {
                        Some(Code2CidValue::Dict(mut sub)) => {
                            add(Rc::make_mut(&mut sub), rest, last, cid)?;
                            d.insert(ci, Code2CidValue::Dict(sub));
                            Ok(())
                        }
                        Some(Code2CidValue::Cid(_)) => Err(non_dict_error(rest.first().copied(), "int")),
                        Some(Code2CidValue::Other(t)) => Err(non_dict_error(rest.first().copied(), t)),
                        None => {
                            let mut t = Code2Cid::new();
                            add(&mut t, rest, last, cid)?;
                            d.insert(ci, Code2CidValue::Dict(Rc::new(t)));
                            Ok(())
                        }
                    }
                }
            }
        }
        /// `ci in d` (or `d[ci] = cid`) on a non-dict `d`.
        fn non_dict_error(next: Option<char>, t: &str) -> PyErr {
            match next {
                Some(_) => PyErr::new(ExcKind::TypeError, format!("argument of type '{t}' is not iterable")),
                None => PyErr::new(ExcKind::TypeError, format!("'{t}' object does not support item assignment")),
            }
        }
        add(Rc::make_mut(&mut self.code2cid), init, *last, cid)
    }

    /// `add_cid2unichr(cid, code)` (`FileUnicodeMap` stores it; the other classes ignore it).
    pub fn add_cid2unichr(&mut self, cid: i64, code: &Value) -> PyResult<()> {
        self.add_cid2unichr_wide(Some(cid), code)
    }

    /// `add_cid2unichr(cid, code)` for a cid that may be beyond i64 (`None`): the code is still
    /// converted (and may raise), but the entry is unreachable and not stored (DIVERGE(bigint):
    /// Python stores it under its big-int key, which only `dump` shows).
    pub fn add_cid2unichr_wide(&mut self, cid: Option<i64>, code: &Value) -> PyResult<()> {
        if self.class != CMapClass::FileUnicodeMap {
            return Ok(());
        }
        let unichr = match code {
            Value::Literal(lit) => {
                // Interpret as an Adobe glyph name.
                if lit.name_str().is_none() {
                    return Err(PyErr::new(ExcKind::AssertionError, ""));
                }
                name2unicode_arg(NameArg::from(&lit.name))?
            }
            Value::Bytes(b) => {
                // Interpret as UTF-16BE.
                PyStr::decode_utf16be_ignore(b)
            }
            Value::Int(_) | Value::Bool(_) => PyStr::chr(code.as_int().unwrap())?,
            other => return Err(PyErr::new(ExcKind::PDFTypeError, other.py_str())),
        };

        let Some(cid) = cid else {
            return Ok(());
        };
        // A0 = non-breaking space, some weird fonts can have a collision on a cid here.
        if unichr.as_bytes() == "\u{a0}".as_bytes() && self.cid2unichr.get(&cid).is_some_and(|u| u.as_bytes() == b" ") {
            return Ok(());
        }
        Rc::make_mut(&mut self.cid2unichr).insert(cid, unichr);
        Ok(())
    }

    /// `use_cmap(cmap)` (`CMap` merges the other cmap's `code2cid`; the others ignore it).
    pub fn use_cmap(&mut self, cmap: &CMapBase) -> PyResult<()> {
        if !self.is_cmap() {
            return Ok(());
        }
        if !cmap.is_cmap() {
            return Err(PyErr::new(
                ExcKind::AssertionError,
                format!("<class 'pdfminer.cmapdb.{}'>", cmap.class.name()),
            ));
        }
        // copy(self.code2cid, cmap.code2cid): the subtrees are shared copy-on-write, which is
        // the same as Python's deep copy.
        let dst = Rc::make_mut(&mut self.code2cid);
        for (k, v) in cmap.code2cid.items() {
            dst.insert(k, v.clone());
        }
        Ok(())
    }

    /// `decode(code)`: the cids of a byte string.
    pub fn decode(&self, code: &[u8]) -> PyResult<Vec<i64>> {
        match self.class {
            CMapClass::CMap | CMapClass::FileCMap | CMapClass::PyCMap => {
                debug!("decode: {}, {}", self.repr(), crate::pyrt::fmt::bytes_repr(code));
                let root: &Code2Cid = &self.code2cid;
                let mut d: Result<&Code2Cid, &'static str> = Ok(root);
                let mut out = Vec::new();
                for &i in code {
                    let cur = match d {
                        Ok(cur) => cur,
                        // `i in d` where `d` is not a dict (not in the pinned data).
                        Err(t) => {
                            return Err(PyErr::new(
                                ExcKind::TypeError,
                                format!("argument of type '{t}' is not iterable"),
                            ));
                        }
                    };
                    match cur.get_int(i as i64) {
                        Some(Code2CidValue::Cid(x)) => {
                            out.push(*x);
                            d = Ok(root);
                        }
                        Some(Code2CidValue::Dict(x)) => d = Ok(x),
                        Some(Code2CidValue::Other(t)) => d = Err(t),
                        None => d = Ok(root),
                    }
                }
                Ok(out)
            }
            CMapClass::IdentityCMap => {
                // struct.unpack(f">{n}H", code[: n * 2]) with n = len(code) // 2
                let (pairs, _) = code.as_chunks::<2>();
                Ok(pairs.iter().map(|c| u16::from_be_bytes(*c) as i64).collect())
            }
            CMapClass::IdentityCMapByte => Ok(code.iter().map(|&b| b as i64).collect()),
            _ => Err(PyErr::new(ExcKind::NotImplementedError, "")),
        }
    }

    /// `get_unichr(cid)` of the unicode maps.
    pub fn get_unichr(&self, cid: i64) -> PyResult<PyStr> {
        match self.class {
            CMapClass::IdentityUnicodeMap => {
                // Interpret character id as unicode codepoint
                debug!("get_unichr: {}, {}", self.repr(), cid);
                PyStr::chr(cid)
            }
            _ if self.is_unicode_map() => {
                debug!("get_unichr: {}, {}", self.repr(), cid);
                self.cid2unichr.get(&cid).cloned().ok_or_else(|| PyErr::new(ExcKind::KeyError, cid.to_string()))
            }
            _ => Err(PyErr::new(
                ExcKind::AttributeError,
                format!("'{}' object has no attribute 'get_unichr'", self.class.name()),
            )),
        }
    }

    /// `dump(out)`: the text `CMap.dump` / `UnicodeMap.dump` writes.
    pub fn dump(&self) -> PyResult<String> {
        let mut out = String::new();
        if self.is_cmap() {
            fn walk(out: &mut String, d: &Code2Cid, code: &mut Vec<i64>) -> PyResult<()> {
                let mut items = d.items();
                if items.iter().any(|(k, _)| matches!(k, Code2CidKey::Str(_))) {
                    return Err(PyErr::new(
                        ExcKind::TypeError,
                        "'<' not supported between instances of 'str' and 'int'",
                    ));
                }
                items.sort_by_key(|(k, _)| match k {
                    Code2CidKey::Int(i) => *i,
                    Code2CidKey::Str(_) => unreachable!(),
                });
                for (k, v) in items {
                    let Code2CidKey::Int(k) = k else { unreachable!() };
                    code.push(k);
                    match v {
                        Code2CidValue::Cid(cid) => {
                            let c: Vec<String> = code.iter().map(|x| x.to_string()).collect();
                            let c = if c.len() == 1 { format!("({},)", c[0]) } else { format!("({})", c.join(", ")) };
                            out.push_str(&format!("code {c} = cid {cid}\n"));
                        }
                        Code2CidValue::Dict(sub) => walk(out, sub, code)?,
                        Code2CidValue::Other(t) => {
                            return Err(PyErr::new(
                                ExcKind::AttributeError,
                                format!("'{t}' object has no attribute 'items'"),
                            ));
                        }
                    }
                    code.pop();
                }
                Ok(())
            }
            walk(&mut out, &self.code2cid, &mut Vec::new())?;
        } else if self.is_unicode_map() {
            let mut items: Vec<(&i64, &PyStr)> = self.cid2unichr.iter().collect();
            items.sort();
            for (k, v) in items {
                out.push_str(&format!("cid {} = unicode {}\n", k, pystr_repr(v)));
            }
        }
        Ok(out)
    }
}

/// The data of a loaded cmap file: Python makes a class with the JSON keys as attributes
/// (`type(name, (), data)`), so a missing key raises `AttributeError` when it is read.
struct CMapModule {
    name: String,
    code2cid: Option<Rc<Code2Cid>>,
    is_vertical: Option<bool>,
    cid2unichr_h: Option<Rc<Cid2Unichr>>,
    cid2unichr_v: Option<Rc<Cid2Unichr>>,
}

impl CMapModule {
    fn missing(&self, attr: &str) -> PyErr {
        PyErr::new(ExcKind::AttributeError, format!("type object '{}' has no attribute '{}'", self.name, attr))
    }
}

/// `PyCMap(name, module)`.
fn py_cmap(name: &str, module: &CMapModule) -> PyResult<CMapBase> {
    let mut cmap = CMapBase::new(CMapClass::PyCMap, &[("CMapName", Value::str(name))]);
    cmap.code2cid = module.code2cid.clone().ok_or_else(|| module.missing("CODE2CID"))?;
    if module.is_vertical.ok_or_else(|| module.missing("IS_VERTICAL"))? {
        cmap.attrs.insert("WMode".into(), Value::Int(1));
    }
    Ok(cmap)
}

/// `PyUnicodeMap(name, module, vertical)`.
fn py_unicode_map(name: &str, module: &CMapModule, vertical: bool) -> PyResult<CMapBase> {
    let mut umap = CMapBase::new(CMapClass::PyUnicodeMap, &[("CMapName", Value::str(name))]);
    if vertical {
        umap.cid2unichr = module.cid2unichr_v.clone().ok_or_else(|| module.missing("CID2UNICHR_V"))?;
        umap.attrs.insert("WMode".into(), Value::Int(1));
    } else {
        umap.cid2unichr = module.cid2unichr_h.clone().ok_or_else(|| module.missing("CID2UNICHR_H"))?;
    }
    Ok(umap)
}

thread_local! {
    // `CMapDB._cmap_cache` / `CMapDB._umap_cache` (per thread: the objects are not `Send`).
    static CMAP_CACHE: RefCell<HashMap<String, Rc<CMapBase>>> = RefCell::new(HashMap::new());
    static UMAP_CACHE: RefCell<HashMap<String, [Rc<CMapBase>; 2]>> = RefCell::new(HashMap::new());
}

/// Number of unknown directories above `.../pdfminer/cmap` in the virtual path of the cmap
/// directory (see [`find_cmap_file`]).
const UNKNOWN_ANCESTORS: usize = 16;

/// The embedded file Python's `_load_data` opens for `name`, if any.
///
/// Python joins `<dir>/<name>.json.gz`, resolves it with `os.path.realpath` (which removes `.`,
/// `..` and repeated slashes lexically, there being no symlinks in the cmap directory) and
/// accepts it when it is a file directly inside the directory. The directory is modelled as
/// `<unknown>/.../pdfminer/cmap`.
fn find_cmap_file(name: &str) -> Option<&'static [u8]> {
    let path = format!("{name}.json.gz");
    if path.starts_with('/') {
        // DIVERGE(cmap-path): an absolute name only reaches the cmap directory through the
        // installed package's real path, which the port does not have.
        return None;
    }
    let ancestors: Vec<String> = (0..UNKNOWN_ANCESTORS).map(|i| format!("\0{i}")).collect();
    let mut base: Vec<&str> = ancestors.iter().map(|s| s.as_str()).collect();
    base.push("pdfminer");
    base.push("cmap");
    let mut stack = base.clone();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            c => stack.push(c),
        }
    }
    if stack.len() != base.len() + 1 || stack[..base.len()] != base[..] {
        return None;
    }
    let file = stack[base.len()];
    // DIVERGE(cmap-case): exact (case-sensitive) file names, as on Linux; on a case-insensitive
    // file system (macOS by default) Python also finds e.g. "h" for "H".
    data::CMAP_FILES.binary_search_by(|(n, _)| n.cmp(&file)).ok().map(|i| data::CMAP_FILES[i].1)
}

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "NoneType",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(n) if n.is_i64() => "int",
        serde_json::Value::Number(_) => "float",
        serde_json::Value::String(_) => "str",
        serde_json::Value::Array(_) => "list",
        serde_json::Value::Object(_) => "dict",
    }
}

/// `int(k)` of a JSON key; `ValueError` when it is not an int.
fn json_key_int(k: &str) -> PyResult<i64> {
    py_int(&Value::str(k))
}

/// The cmap JSON file names are not user data: the pinned files only hold what this converts
/// (checked by the differential test of every embedded cmap).
fn json_shape_error(name: &str, what: &str) -> PyErr {
    PyErr::new(ExcKind::TypeError, format!("unexpected {what} in cmap data {name}"))
}

pub struct CMapDB;

impl CMapDB {
    /// Recursively convert string keys to integers in CODE2CID dictionaries.
    fn _convert_code2cid_keys(d: &serde_json::Map<String, serde_json::Value>) -> Code2Cid {
        let mut result = Code2Cid::new();
        for (k, v) in d {
            // Convert string keys to integers
            let new_key = match json_key_int(k) {
                Ok(i) => Code2CidKey::Int(i),
                Err(_) => Code2CidKey::Str(Rc::from(k.as_str())),
            };
            // Recursively convert nested dictionaries
            let value = match v {
                serde_json::Value::Object(o) => Code2CidValue::Dict(Rc::new(CMapDB::_convert_code2cid_keys(o))),
                serde_json::Value::Number(n) if n.is_i64() => Code2CidValue::Cid(n.as_i64().unwrap()),
                serde_json::Value::Bool(b) => Code2CidValue::Cid(*b as i64),
                other => Code2CidValue::Other(json_type_name(other)),
            };
            result.insert(new_key, value);
        }
        result
    }

    fn _convert_cid2unichr(name: &str, v: &serde_json::Value) -> PyResult<Rc<Cid2Unichr>> {
        let serde_json::Value::Object(o) = v else {
            return Err(json_shape_error(name, "CID2UNICHR"));
        };
        let mut out = HashMap::with_capacity(o.len());
        for (k, v) in o {
            let serde_json::Value::String(s) = v else {
                return Err(json_shape_error(name, "CID2UNICHR value"));
            };
            out.insert(json_key_int(k)?, PyStr::from(s.as_str()));
        }
        Ok(Rc::new(out))
    }

    fn _load_data(name: &str) -> PyResult<CMapModule> {
        let name = name.replace('\0', "");
        debug!("loading: {name:?}");
        // DIVERGE(cmap-path): Python first looks in `$CMAP_PATH` (default `/usr/share/pdfminer/`);
        // the port only has the embedded files of `pdfminer/cmap`.
        let Some(gz) = find_cmap_file(&name) else {
            return Err(cmap_not_found(&name));
        };
        debug!("loading JSON: {name:?}");
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(gz)
            .read_to_end(&mut raw)
            .map_err(|e| PyErr::new(ExcKind::OSError, e.to_string()))?;
        let data: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|e| PyErr::new(ExcKind::ValueError, e.to_string()))?;
        let serde_json::Value::Object(data) = data else {
            return Err(json_shape_error(&name, "top-level value"));
        };
        let mut module = CMapModule {
            name: name.clone(),
            code2cid: None,
            is_vertical: None,
            cid2unichr_h: None,
            cid2unichr_v: None,
        };
        // Convert string keys to integers for CID mappings
        if let Some(v) = data.get("CID2UNICHR_H") {
            module.cid2unichr_h = Some(CMapDB::_convert_cid2unichr(&name, v)?);
        }
        if let Some(v) = data.get("CID2UNICHR_V") {
            module.cid2unichr_v = Some(CMapDB::_convert_cid2unichr(&name, v)?);
        }
        // CODE2CID may also have numeric keys that need conversion
        if let Some(v) = data.get("CODE2CID") {
            match v {
                serde_json::Value::Object(o) => {
                    module.code2cid = Some(Rc::new(CMapDB::_convert_code2cid_keys(o)));
                }
                _ => return Err(json_shape_error(&name, "CODE2CID")),
            }
        }
        if let Some(v) = data.get("IS_VERTICAL") {
            // `if module.IS_VERTICAL:` (JSON truthiness)
            module.is_vertical = Some(match v {
                serde_json::Value::Null => false,
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::Number(n) => n.as_f64() != Some(0.0),
                serde_json::Value::String(s) => !s.is_empty(),
                serde_json::Value::Array(a) => !a.is_empty(),
                serde_json::Value::Object(o) => !o.is_empty(),
            });
        }
        Ok(module)
    }

    /// `CMapDB.get_cmap(name)`.
    pub fn get_cmap(name: &str) -> PyResult<Rc<CMapBase>> {
        match name {
            "Identity-H" => {
                return Ok(Rc::new(CMapBase::new(CMapClass::IdentityCMap, &[("WMode", Value::Int(0))])));
            }
            "Identity-V" => {
                return Ok(Rc::new(CMapBase::new(CMapClass::IdentityCMap, &[("WMode", Value::Int(1))])));
            }
            "OneByteIdentityH" => {
                return Ok(Rc::new(CMapBase::new(CMapClass::IdentityCMapByte, &[("WMode", Value::Int(0))])));
            }
            "OneByteIdentityV" => {
                return Ok(Rc::new(CMapBase::new(CMapClass::IdentityCMapByte, &[("WMode", Value::Int(1))])));
            }
            _ => {}
        }
        if let Some(c) = CMAP_CACHE.with(|c| c.borrow().get(name).cloned()) {
            return Ok(c);
        }
        let data = CMapDB::_load_data(name)?;
        let cmap = Rc::new(py_cmap(name, &data)?);
        CMAP_CACHE.with(|c| c.borrow_mut().insert(name.to_string(), cmap.clone()));
        Ok(cmap)
    }

    /// `CMapDB.get_unicode_map(name, vertical)`.
    pub fn get_unicode_map(name: &str, vertical: bool) -> PyResult<Rc<CMapBase>> {
        if let Some(m) = UMAP_CACHE.with(|c| c.borrow().get(name).cloned()) {
            return Ok(m[vertical as usize].clone());
        }
        let data = CMapDB::_load_data(&format!("to-unicode-{name}"))?;
        let maps = [Rc::new(py_unicode_map(name, &data, false)?), Rc::new(py_unicode_map(name, &data, true)?)];
        let m = maps[vertical as usize].clone();
        UMAP_CACHE.with(|c| c.borrow_mut().insert(name.to_string(), maps));
        Ok(m)
    }

    /// The names of the embedded cmap files (without `.json.gz`).
    pub fn embedded_names() -> Vec<&'static str> {
        data::CMAP_FILES.iter().map(|(n, _)| n.strip_suffix(".json.gz").unwrap()).collect()
    }
}

/// `nunpack(s)` exactly, as an `i128`, for byte strings of up to 15 significant bytes; `None`
/// beyond. DIVERGE(bigint): Python's value is unbounded.
pub(crate) fn nunpack_wide(s: &[u8]) -> Option<i128> {
    let s = &s[s.iter().position(|&b| b != 0).unwrap_or(s.len())..];
    if s.len() > 15 {
        return None;
    }
    Some(s.iter().fold(0i128, |v, &b| (v << 8) | b as i128))
}

/// `nunpack(a) - nunpack(b)` for two byte strings of the same length, exact whenever the
/// difference fits; a difference too large for an `i128` saturates (a range that long never
/// finishes in Python either).
fn be_sub(a: &[u8], b: &[u8]) -> i128 {
    debug_assert_eq!(a.len(), b.len());
    let Some(k) = a.iter().zip(b).position(|(x, y)| x != y) else {
        return 0;
    };
    match (nunpack_wide(&a[k..]), nunpack_wide(&b[k..])) {
        (Some(x), Some(y)) => x - y,
        _ => {
            if a[k] > b[k] {
                i128::MAX / 2
            } else {
                i128::MIN / 2
            }
        }
    }
}

/// A cid computed from a code: `None` when it is beyond i64 (Python stores it under a big-int
/// key that no cid can look up).
fn wide_cid(v: Option<i128>) -> Option<i64> {
    v.and_then(|v| i64::try_from(v).ok())
}

/// `struct.pack(">L", v)`.
fn pack_ulong(v: i128) -> PyResult<[u8; 4]> {
    if !(0..=0xFFFF_FFFF).contains(&v) {
        return Err(PyErr::new(ExcKind::StructError, "'L' format requires 0 <= number <= 4294967295"));
    }
    Ok((v as u32).to_be_bytes())
}

/// `prefix + struct.pack(">L", v)[-vlen:]` (`[-0:]` is the whole packed value).
fn prefixed(prefix: &[u8], v: i128, vlen: usize) -> PyResult<Value> {
    let packed = pack_ulong(v)?;
    let tail = if vlen == 0 { &packed[..] } else { &packed[4 - vlen.min(4)..] };
    let mut x = prefix.to_vec();
    x.extend_from_slice(tail);
    Ok(Value::bytes(&x))
}

/// `b[:-4]`, `b[-4:]`.
fn split4(b: &[u8]) -> (&[u8], &[u8]) {
    b.split_at(b.len().saturating_sub(4))
}

/// `((_, a), (_, b), ...) = self.pop(n)`.
fn unpack_popped(popped: Vec<(i64, Value)>, n: usize) -> PyResult<Vec<Value>> {
    if popped.len() != n {
        return Err(PyErr::new(
            ExcKind::ValueError,
            format!("not enough values to unpack (expected {n}, got {})", popped.len()),
        ));
    }
    Ok(popped.into_iter().map(|(_, v)| v).collect())
}

/// `CMapParser`.
pub struct CMapParser<'a> {
    pub ps: PSStackParser,
    pub cmap: &'a mut CMapBase,
    // some ToUnicode maps don't have "begincmap" keyword.
    _in_cmap: bool,
    _warnings: HashSet<&'static str>,
}

impl<'a> CMapParser<'a> {
    pub fn new(cmap: &'a mut CMapBase, fp: BytesIO) -> Self {
        CMapParser { ps: PSStackParser::new(Some(fp)), cmap, _in_cmap: true, _warnings: HashSet::new() }
    }

    pub fn run(&mut self) -> PyResult<()> {
        match self.nextobject() {
            Ok(_) => Ok(()),
            Err(e) if e.is(ExcKind::PSEOF) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Warn once for each unique message
    fn _warn_once(&mut self, msg: &'static str) {
        if self._warnings.insert(msg) {
            let base_msg = "Ignoring (part of) ToUnicode map because the PDF data \
                            does not conform to the format. This could result in \
                            (cid) values in the output. ";
            warn!("{base_msg}{msg}");
        }
    }

    fn objs(&mut self) -> Vec<Value> {
        self.ps.popall().into_iter().map(|(_, obj)| obj).collect()
    }

    /// ToUnicode CMaps
    ///
    /// See Section 5.9.2 - ToUnicode CMaps of the PDF Reference.
    fn cmap_do_keyword(&mut self, pos: i64, token: PSKeyword) -> PyResult<()> {
        if token.is(b"begincmap") {
            self._in_cmap = true;
            self.ps.popall();
            return Ok(());
        } else if token.is(b"endcmap") {
            self._in_cmap = false;
            return Ok(());
        }

        if !self._in_cmap {
            return Ok(());
        }

        if token.is(b"def") {
            // `except PSSyntaxError: pass` (nothing here raises it)
            let kv = unpack_popped(self.ps.pop(2), 2)?;
            self.cmap.set_attr(literal_name(&kv[0])?, kv[1].clone());
            return Ok(());
        }

        if token.is(b"usecmap") {
            let v = unpack_popped(self.ps.pop(1), 1)?;
            match CMapDB::get_cmap(&literal_name(&v[0])?) {
                Ok(cmap) => self.cmap.use_cmap(&cmap)?,
                Err(e) if is_cmap_not_found(&e) || e.is(ExcKind::PSSyntaxError) => {}
                Err(e) => return Err(e),
            }
            return Ok(());
        }

        if token.is(b"begincodespacerange") || token.is(b"endcodespacerange") {
            self.ps.popall();
            return Ok(());
        }

        if token.is(b"begincidrange") {
            self.ps.popall();
            return Ok(());
        }

        if token.is(b"endcidrange") {
            let objs = self.objs();
            for t in choplist(3, objs) {
                let (start_byte, end_byte, cid) = (&t[0], &t[1], &t[2]);
                let Value::Bytes(start_byte) = start_byte else {
                    self._warn_once("The start object of begincidrange is not a byte.");
                    continue;
                };
                let Value::Bytes(end_byte) = end_byte else {
                    self._warn_once("The end object of begincidrange is not a byte.");
                    continue;
                };
                let Some(cid) = cid.as_int() else {
                    self._warn_once("The cid object of begincidrange is not a byte.");
                    continue;
                };
                if start_byte.len() != end_byte.len() {
                    self._warn_once("The start and end byte of begincidrange have different lengths.");
                    continue;
                }
                let (start_prefix, svar) = split4(start_byte);
                let (end_prefix, evar) = split4(end_byte);
                if start_prefix != end_prefix {
                    self._warn_once("The prefix of the start and end byte of begincidrange are not the same.");
                    continue;
                }
                let start = nunpack(svar, 0);
                let end = nunpack(evar, 0);
                let vlen = svar.len();
                for i in 0..(end - start + 1).max(0) {
                    let x = prefixed(start_prefix, (start + i) as i128, vlen)?;
                    self.cmap.add_cid2unichr_wide(wide_cid(Some(cid as i128 + i as i128)), &x)?;
                }
            }
            return Ok(());
        }

        if token.is(b"begincidchar") {
            self.ps.popall();
            return Ok(());
        }

        if token.is(b"endcidchar") {
            let objs = self.objs();
            for t in choplist(2, objs) {
                let (cid, code) = (&t[0], &t[1]);
                if let (Value::Bytes(_), Some(cid)) = (code, cid.as_int()) {
                    self.cmap.add_cid2unichr(cid, code)?;
                }
            }
            return Ok(());
        }

        if token.is(b"beginbfrange") {
            self.ps.popall();
            return Ok(());
        }

        if token.is(b"endbfrange") {
            let objs = self.objs();
            for t in choplist(3, objs) {
                let (start_byte, end_byte, code) = (&t[0], &t[1], &t[2]);
                let Value::Bytes(start_byte) = start_byte else {
                    self._warn_once("The start object is not a byte.");
                    continue;
                };
                let Value::Bytes(end_byte) = end_byte else {
                    self._warn_once("The end object is not a byte.");
                    continue;
                };
                if start_byte.len() != end_byte.len() {
                    self._warn_once("The start and end byte have different lengths.");
                    continue;
                }
                // `start`, `end` are Python ints of any size: exact in i128 up to 15 bytes, and
                // `end - start` exact whenever it fits.
                let start = nunpack_wide(start_byte);
                let count = (be_sub(end_byte, start_byte) + 1).max(0);
                if let Value::List(code) = code {
                    let code = code.borrow().clone();
                    if code.len() as i128 != count {
                        self._warn_once(
                            "The difference between the start and end offsets does not match the code length.",
                        );
                    }
                    for (i, unicode_value) in code.iter().enumerate().take(count.min(usize::MAX as i128) as usize) {
                        let cid = start.map(|s| s + i as i128);
                        self.cmap.add_cid2unichr_wide(wide_cid(cid), unicode_value)?;
                    }
                } else {
                    let Value::Bytes(code) = code else {
                        return Err(PyErr::new(ExcKind::AssertionError, ""));
                    };
                    let (prefix, var) = split4(code);
                    let base = nunpack(var, 0) as i128;
                    let vlen = var.len();
                    let mut i: i128 = 0;
                    while i < count {
                        let x = prefixed(prefix, base + i, vlen)?;
                        self.cmap.add_cid2unichr_wide(wide_cid(start.map(|s| s + i)), &x)?;
                        i += 1;
                    }
                }
            }
            return Ok(());
        }

        if token.is(b"beginbfchar") {
            self.ps.popall();
            return Ok(());
        }

        if token.is(b"endbfchar") {
            let objs = self.objs();
            for t in choplist(2, objs) {
                if let (Value::Bytes(cid), Value::Bytes(_)) = (&t[0], &t[1]) {
                    self.cmap.add_cid2unichr_wide(wide_cid(nunpack_wide(cid)), &t[1])?;
                }
            }
            return Ok(());
        }

        if token.is(b"beginnotdefrange") || token.is(b"endnotdefrange") {
            self.ps.popall();
            return Ok(());
        }

        self.ps.push((pos, Value::Keyword(token)));
        Ok(())
    }
}

impl PSStackParserImpl for CMapParser<'_> {
    fn ps(&self) -> &PSStackParser {
        &self.ps
    }
    fn ps_mut(&mut self) -> &mut PSStackParser {
        &mut self.ps
    }
    fn do_keyword(&mut self, pos: i64, token: PSKeyword) -> PyResult<()> {
        self.cmap_do_keyword(pos, token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be16(vals: impl IntoIterator<Item = u16>) -> Vec<u8> {
        vals.into_iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    fn identity() -> CMapBase {
        CMapBase::new(CMapClass::IdentityCMap, &[])
    }

    // Port of tests/test_cmapdb.py: IdentityCMap decoding.

    /// Regression test for issues #668, #785, #193 (odd-length buffers).
    #[test]
    fn test_identity_cmap_odd_length_buffer() {
        let mut odd_buffer = be16(0..10);
        odd_buffer.push(0);
        assert_eq!(odd_buffer.len(), 21);
        let result = identity().decode(&odd_buffer).unwrap();
        assert_eq!(result, (0..10).collect::<Vec<i64>>());
    }

    #[test]
    fn test_identity_cmap_even_length_buffer() {
        let even_buffer = be16(0..10);
        assert_eq!(even_buffer.len(), 20);
        assert_eq!(identity().decode(&even_buffer).unwrap(), (0..10).collect::<Vec<i64>>());
    }

    #[test]
    fn test_identity_cmap_empty_buffer() {
        assert!(identity().decode(b"").unwrap().is_empty());
    }

    #[test]
    fn test_identity_cmap_single_byte_buffer() {
        assert!(identity().decode(b"\x00").unwrap().is_empty());
    }

    #[test]
    fn test_identity_cmap_byte_odd_length() {
        let cmap_byte = CMapBase::new(CMapClass::IdentityCMapByte, &[]);
        for length in [1usize, 5, 11, 13, 21, 255] {
            let test_buffer: Vec<u8> = (0..length).map(|i| i as u8).collect();
            let result = cmap_byte.decode(&test_buffer).unwrap();
            assert_eq!(result, (0..length as i64).collect::<Vec<i64>>());
        }
    }

    #[test]
    fn test_identity_cmap_various_odd_lengths() {
        for num_shorts in 1..11u16 {
            let mut buffer = be16(0..num_shorts);
            buffer.push(0);
            assert_eq!(buffer.len(), num_shorts as usize * 2 + 1);
            let result = identity().decode(&buffer).unwrap();
            assert_eq!(result, (0..num_shorts as i64).collect::<Vec<i64>>());
        }
    }

    #[test]
    fn test_identity_cmap_max_values() {
        let max_values = [65535u16, 65534, 65533, 65532, 65531];
        let result = identity().decode(&be16(max_values)).unwrap();
        assert_eq!(result, max_values.iter().map(|&v| v as i64).collect::<Vec<i64>>());
    }

    // Port of tests/test_cmapdb_security.py (CVE-2025-64512).

    #[test]
    fn test_json_format_loading() {
        let cmap = CMapDB::get_cmap("H").unwrap();
        assert_eq!(cmap.repr(), "<CMap: H>");
    }

    #[test]
    fn test_unicode_map_json_loading() {
        let umap = CMapDB::get_unicode_map("Adobe-Japan1", false).unwrap();
        assert_eq!(umap.repr(), "<UnicodeMap: Adobe-Japan1>");
        // Check a known mapping (CID 1 should map to space)
        assert_eq!(umap.cid2unichr.get(&1).unwrap().as_str(), Some(" "));
    }

    #[test]
    fn test_code2cid_key_conversion() {
        let cmap = CMapDB::get_cmap("H").unwrap();
        fn check_keys(d: &Code2Cid) {
            for (k, v) in d.items() {
                assert!(matches!(k, Code2CidKey::Int(_)), "Expected int key, got {k:?}");
                if let Code2CidValue::Dict(sub) = v {
                    check_keys(sub);
                }
            }
        }
        assert!(!cmap.code2cid.is_empty());
        check_keys(&cmap.code2cid);
    }

    #[test]
    fn path_resolution_like_realpath() {
        for (name, found) in [
            ("H", true),
            ("sub/../H", true),
            ("H.json.gz/../H", true),
            ("../cmap/H", true),
            ("./H", true),
            ("/H", false),
            ("H/", false),
            ("a//H", false),
            ("..", false),
            ("../../pdfminer/cmap/H", true),
            ("../../../x/pdfminer/cmap/H", false),
            ("H\0", true),
            ("nonexistent", false),
        ] {
            let r = CMapDB::get_cmap(name);
            assert_eq!(r.is_ok(), found, "{name:?}");
            if !found {
                assert!(is_cmap_not_found(&r.unwrap_err()));
            }
        }
        // A unicode map file read as a cmap has no CODE2CID.
        assert!(CMapDB::get_cmap("to-unicode-Adobe-Japan1").unwrap_err().is(ExcKind::AttributeError));
        assert!(CMapDB::get_unicode_map("x/../../cmap/H", false).unwrap_err().is(ExcKind::AttributeError));
        assert_eq!(CMapDB::embedded_names().len(), 148);
    }

    fn parse_unicode_map(data: &[u8]) -> PyResult<CMapBase> {
        let mut m = CMapBase::new_file_unicode_map();
        CMapParser::new(&mut m, BytesIO::from_vec(data.to_vec())).run()?;
        Ok(m)
    }

    #[test]
    fn tounicode_cmap_parsing() {
        let data = b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap
/CMapName /Adobe-Identity-UCS def
1 begincodespacerange <0000> <FFFF> endcodespacerange
2 beginbfchar <0003> <0020> <0004> <00410042> endbfchar
2 beginbfrange <0010> <0012> <0061> <0020> <0021> [<0031> /two] endbfrange
1 begincidrange <0100> <0101> 50 endcidrange
2 begincidchar 7 <0063> <0064> 8 endcidchar
endcmap CMapName currentdict /CMap defineresource pop end end";
        let m = parse_unicode_map(data).unwrap();
        let get = |c: i64| m.get_unichr(c).ok().map(|s| s.to_string());
        assert_eq!(get(3).as_deref(), Some(" "));
        assert_eq!(get(4).as_deref(), Some("AB"));
        assert_eq!(get(0x10).as_deref(), Some("a"));
        assert_eq!(get(0x12).as_deref(), Some("c"));
        assert_eq!(get(0x20).as_deref(), Some("1"));
        assert_eq!(get(0x21).as_deref(), Some("2"));
        assert_eq!(get(50).as_deref(), Some("\u{100}"));
        assert_eq!(get(51).as_deref(), Some("\u{101}"));
        // cidchar pairs are read as (cid, code): only the "int bytes" pair is used.
        assert_eq!(get(7).as_deref(), Some("c"));
        assert_eq!(get(0x64), None);
        assert_eq!(m.attrs.get("CMapName").unwrap().repr(), "/'Adobe-Identity-UCS'");
    }

    #[test]
    fn tounicode_cmap_errors() {
        // `def` with one object on the stack: ValueError from the unpacking.
        assert!(parse_unicode_map(b"begincmap /A def").unwrap_err().is(ExcKind::ValueError));
        // bfrange past 0xFFFFFFFF: struct.error.
        assert!(parse_unicode_map(b"<00> <02> <FFFFFFFF> endbfrange").unwrap_err().is(ExcKind::StructError));
        // bfrange whose code is neither bytes nor a list: AssertionError.
        assert!(parse_unicode_map(b"<00> <02> 5 endbfrange").unwrap_err().is(ExcKind::AssertionError));
        // An empty start/end: `[-0:]` keeps the four packed bytes.
        let m = parse_unicode_map(b"<> <> 9 endcidrange").unwrap();
        assert_eq!(m.get_unichr(9).unwrap().to_utf16(), vec![0, 0]);
        // A0 after a space is ignored.
        let m = parse_unicode_map(b"<01> <0020> <01> <00A0> endbfchar").unwrap();
        assert_eq!(m.get_unichr(1).unwrap().as_str(), Some(" "));
        // After endcmap, everything is ignored.
        let m = parse_unicode_map(b"endcmap <01> <0041> endbfchar").unwrap();
        assert!(m.cid2unichr.is_empty());
    }

    #[test]
    fn file_cmap_and_use_cmap() {
        let mut c = CMapBase::new(CMapClass::FileCMap, &[]);
        c.add_code2cid("\u{81}\u{40}", 633).unwrap();
        c.add_code2cid("A", 34).unwrap();
        assert_eq!(c.decode(b"\x81\x40A\x81B").unwrap(), vec![633, 34]);
        assert!(c.add_code2cid("A\u{1}", 1).unwrap_err().is(ExcKind::TypeError));
        let mut c2 = CMapBase::new(CMapClass::FileCMap, &[]);
        let parsed = {
            let mut p = CMapParser::new(&mut c2, BytesIO::from_vec(b"/H usecmap".to_vec()));
            p.run()
        };
        parsed.unwrap();
        assert_eq!(c2.decode(b"\x21\x21").unwrap(), CMapDB::get_cmap("H").unwrap().decode(b"\x21\x21").unwrap());
        assert_eq!(c2.dump().unwrap(), CMapDB::get_cmap("H").unwrap().dump().unwrap());
        let mut c3 = CMapBase::new(CMapClass::FileCMap, &[]);
        let r = CMapParser::new(&mut c3, BytesIO::from_vec(b"/Identity-H usecmap".to_vec())).run();
        assert!(r.unwrap_err().is(ExcKind::AssertionError));
    }
}
