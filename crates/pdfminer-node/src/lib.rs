//! `@fuelfwd/pdfminer`: the Node binding of the port (napi-rs).
//!
//! The native functions take the PDF (a `Buffer`, or a path the port opens like Python's
//! `open(path, "rb")`) and an options object, run pdfminer on a thread of their own (with a
//! stack deep enough for Python's recursion limit) and return:
//!
//! - `extractText`: `high_level.extract_text`, a string (lone surrogates kept: JS strings hold
//!   them too);
//! - `extractPages`: `high_level.extract_pages`, every page as a plain object in the format of
//!   `tools/oracle.py` (`pdfminer::dump`), so it can be compared with Python directly;
//! - `convert`: `high_level.extract_text_to_fp` into a `Buffer` (text, html, xml, hocr, tag);
//! - `dumpLayout`: the canonical layout dump text of `pdfminer-rs dump`.
//!
//! Each has an `...Async` variant returning a `Promise` (computed off the main thread). A Python
//! exception becomes a JS `Error` whose `code` is the exception class (`PDFSyntaxError`) and
//! whose `message` is `str(e)`; `index.js` turns it into a `PdfminerError`.
//!
//! In options, `undefined` (or a missing key) is the Python default and `null` is `None`.

use std::collections::HashSet;
use std::rc::Rc;

use napi::bindgen_prelude::*;
use napi::sys;
use napi_derive::napi;
use pdfminer::dump::Node;
use pdfminer::high_level::{ExtractTextToFp, FileOrName, extract_pages, extract_text, extract_text_to_fp, read_file};
use pdfminer::layout::LAParams;
use pdfminer::pyrt::PyStr;
use pdfminer::pyrt::error::PyErr;
use pdfminer::pyrt::textio::BytesOut;

/// The stack of the threads the work runs on: Python's 1000 frames of recursion (deeply nested
/// forms, text groups) need more than Node's main thread or a Windows thread has.
const STACK_SIZE: usize = 256 << 20;

/// Runs `f` on a thread with a [`STACK_SIZE`] stack.
fn on_big_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Result<R> {
    std::thread::Builder::new()
        .name("pdfminer".into())
        .stack_size(STACK_SIZE)
        .spawn(f)
        .map_err(|e| Error::from_reason(format!("cannot start a thread: {e}")))?
        .join()
        .map_err(|_| Error::from_reason("pdfminer panicked"))
}

/// A Python exception as a JS error: `code` is the class name, `message` is `str(e)`.
fn py_error(e: PyErr) -> Error {
    Error::new(Status::GenericFailure, e.msg).with_code(e.kind.name())
}

trait WithCode {
    fn with_code(self, code: &str) -> Self;
}

impl WithCode for Error {
    /// napi's `Error` has a status, not a free-form code: keep the class in the reason with a
    /// marker `index.js` strips (`\u0000Class\u0000message`).
    fn with_code(self, code: &str) -> Self {
        Error::new(Status::GenericFailure, format!("\u{0}{code}\u{0}{}", self.reason))
    }
}

fn type_error(msg: impl Into<String>) -> Error {
    Error::new(Status::InvalidArg, msg.into())
}

// ---------------------------------------------------------------------------------------------
// Input and options

/// The PDF: its bytes, or a path.
enum Source {
    Data(Vec<u8>),
    Path(String),
}

impl Source {
    fn from_js(pdf: Either<Buffer, String>) -> Source {
        match pdf {
            Either::A(b) => Source::Data(b.to_vec()),
            Either::B(p) => Source::Path(p),
        }
    }

    fn file_or_name(self) -> FileOrName {
        match self {
            Source::Data(d) => FileOrName::Data(Rc::from(d)),
            Source::Path(p) => FileOrName::Path(p),
        }
    }

    /// `open(path, "rb")`: the bytes.
    fn data(self) -> std::result::Result<Rc<[u8]>, PyErr> {
        match self {
            Source::Data(d) => Ok(Rc::from(d)),
            Source::Path(p) => read_file(&p),
        }
    }
}

/// An option: missing (`undefined`), `null`, or a value.
enum Opt<T> {
    Missing,
    Null,
    Some(T),
}

impl<T> Opt<T> {
    fn or(self, default: Option<T>) -> Option<T> {
        match self {
            Opt::Missing => default,
            Opt::Null => None,
            Opt::Some(v) => Some(v),
        }
    }
}

fn get<T: FromNapiValue>(o: &Object, key: &str, want: ValueType, what: &str) -> Result<Opt<T>> {
    let Some(v) = o.get::<Unknown>(key)? else {
        return Ok(Opt::Missing);
    };
    let ty = v.get_type()?;
    if ty == ValueType::Null {
        return Ok(Opt::Null);
    }
    if ty != want {
        return Err(type_error(format!("option {key} must be {what}")));
    }
    Ok(Opt::Some(o.get::<T>(key)?.expect("present")))
}

fn get_f64(o: &Object, key: &str) -> Result<Opt<f64>> {
    get(o, key, ValueType::Number, "a number")
}

fn get_bool(o: &Object, key: &str) -> Result<Opt<bool>> {
    get(o, key, ValueType::Boolean, "a boolean")
}

fn get_str(o: &Object, key: &str) -> Result<Opt<String>> {
    get(o, key, ValueType::String, "a string")
}

/// An int option (a JS number with an integral value).
fn get_int(o: &Object, key: &str) -> Result<Opt<i64>> {
    Ok(match get_f64(o, key)? {
        Opt::Missing => Opt::Missing,
        Opt::Null => Opt::Null,
        Opt::Some(f) if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 => Opt::Some(f as i64),
        Opt::Some(_) => return Err(type_error(format!("option {key} must be an integer"))),
    })
}

/// `pageNumbers`: 0-based page numbers (Python's `page_numbers`), `null` or missing for all.
fn get_page_numbers(o: &Object) -> Result<Option<HashSet<i64>>> {
    let Some(v) = o.get::<Unknown>("pageNumbers")? else {
        return Ok(None);
    };
    if v.get_type()? == ValueType::Null {
        return Ok(None);
    }
    let nums: Vec<f64> = o
        .get::<Vec<f64>>("pageNumbers")
        .map_err(|_| type_error("option pageNumbers must be an array of integers"))?
        .expect("present");
    nums.into_iter()
        .map(|f| {
            if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 {
                Ok(f as i64)
            } else {
                Err(type_error("option pageNumbers must be an array of integers"))
            }
        })
        .collect::<Result<HashSet<i64>>>()
        .map(Some)
}

/// `laparams`: an object with Python's `LAParams` fields in camelCase (`boxesFlow: null` is
/// `boxes_flow=None`), or `null` for no layout analysis; missing gives the function's default.
fn get_laparams(o: &Object, default: Option<LAParams>) -> Result<Option<LAParams>> {
    let Some(v) = o.get::<Unknown>("laparams")? else {
        return Ok(default);
    };
    match v.get_type()? {
        ValueType::Null => return Ok(None),
        ValueType::Object => {}
        _ => return Err(type_error("option laparams must be an object or null")),
    }
    let p: Object = o.get("laparams")?.expect("present");
    let d = LAParams::default();
    let la = LAParams {
        line_overlap: get_f64(&p, "lineOverlap")?.or(Some(d.line_overlap)).unwrap_or(d.line_overlap),
        char_margin: get_f64(&p, "charMargin")?.or(Some(d.char_margin)).unwrap_or(d.char_margin),
        line_margin: get_f64(&p, "lineMargin")?.or(Some(d.line_margin)).unwrap_or(d.line_margin),
        word_margin: get_f64(&p, "wordMargin")?.or(Some(d.word_margin)).unwrap_or(d.word_margin),
        boxes_flow: get_f64(&p, "boxesFlow")?.or(d.boxes_flow),
        detect_vertical: get_bool(&p, "detectVertical")?.or(Some(d.detect_vertical)).unwrap_or(false),
        all_texts: get_bool(&p, "allTexts")?.or(Some(d.all_texts)).unwrap_or(false),
    };
    la._validate().map_err(py_error)?;
    Ok(Some(la))
}

/// The options of every function, with the defaults of the Python function it mirrors.
#[derive(Clone)]
struct Options {
    password: String,
    page_numbers: Option<HashSet<i64>>,
    maxpages: i64,
    caching: bool,
    codec: String,
    laparams: Option<LAParams>,
    output_type: String,
    scale: f64,
    rotation: i64,
    layoutmode: String,
    output_dir: Option<String>,
    strip_control: bool,
}

fn options(o: Option<Object>, default_laparams: Option<LAParams>) -> Result<Options> {
    let mut r = Options {
        password: String::new(),
        page_numbers: None,
        maxpages: 0,
        caching: true,
        codec: "utf-8".into(),
        laparams: default_laparams.clone(),
        output_type: "text".into(),
        scale: 1.0,
        rotation: 0,
        layoutmode: "normal".into(),
        output_dir: None,
        strip_control: false,
    };
    let Some(o) = o else { return Ok(r) };
    r.password = get_str(&o, "password")?.or(Some(r.password)).unwrap_or_default();
    r.page_numbers = get_page_numbers(&o)?;
    r.maxpages = get_int(&o, "maxpages")?.or(Some(0)).unwrap_or(0);
    r.caching = get_bool(&o, "caching")?.or(Some(true)).unwrap_or(false);
    // `codec: null` is Python's None (the HTML/XML/hOCR converters then write str).
    r.codec = get_str(&o, "codec")?.or(Some(r.codec)).unwrap_or_default();
    r.laparams = get_laparams(&o, default_laparams)?;
    r.output_type = get_str(&o, "outputType")?.or(Some(r.output_type)).unwrap_or_default();
    r.scale = get_f64(&o, "scale")?.or(Some(1.0)).unwrap_or(1.0);
    r.rotation = get_int(&o, "rotation")?.or(Some(0)).unwrap_or(0);
    r.layoutmode = get_str(&o, "layoutmode")?.or(Some(r.layoutmode)).unwrap_or_default();
    r.output_dir = get_str(&o, "outputDir")?.or(None);
    r.strip_control = get_bool(&o, "stripControl")?.or(Some(false)).unwrap_or(false);
    Ok(r)
}

// ---------------------------------------------------------------------------------------------
// Results

/// A Python `str` as a JS string (UTF-16, lone surrogates kept).
pub struct JsText(PyStr);

impl TypeName for JsText {
    fn type_name() -> &'static str {
        "string"
    }
    fn value_type() -> ValueType {
        ValueType::String
    }
}

impl ToNapiValue for JsText {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        unsafe { utf16(env, &val.0) }
    }
}

unsafe fn utf16(env: sys::napi_env, s: &PyStr) -> Result<sys::napi_value> {
    let units = s.to_utf16();
    let mut out = std::ptr::null_mut();
    check(unsafe { sys::napi_create_string_utf16(env, units.as_ptr(), units.len() as isize, &mut out) })?;
    Ok(out)
}

fn check(status: sys::napi_status) -> Result<()> {
    if status == sys::Status::napi_ok { Ok(()) } else { Err(Error::new(Status::from(status), "napi call failed")) }
}

/// A dump value as a JS value: objects, arrays, numbers (`NaN`, `Infinity` included), strings,
/// booleans and `null`.
pub struct JsNode(Node);

impl TypeName for JsNode {
    fn type_name() -> &'static str {
        "object"
    }
    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for JsNode {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        unsafe { node(env, &val.0) }
    }
}

unsafe fn node(env: sys::napi_env, n: &Node) -> Result<sys::napi_value> {
    let mut out = std::ptr::null_mut();
    unsafe {
        match n {
            Node::Null => check(sys::napi_get_null(env, &mut out))?,
            Node::Bool(b) => check(sys::napi_get_boolean(env, *b, &mut out))?,
            // A JS number: exact up to 2**53, like JSON.parse.
            Node::Int(i) => check(sys::napi_create_double(env, *i as f64, &mut out))?,
            Node::Float(f) => check(sys::napi_create_double(env, *f, &mut out))?,
            Node::Str(s) => out = utf16(env, s)?,
            Node::List(items) => {
                check(sys::napi_create_array_with_length(env, items.len(), &mut out))?;
                for (i, x) in items.iter().enumerate() {
                    check(sys::napi_set_element(env, out, i as u32, node(env, x)?))?;
                }
            }
            Node::Object(fields) => {
                check(sys::napi_create_object(env, &mut out))?;
                for (k, x) in fields {
                    let key = std::ffi::CString::new(*k).expect("no NUL in keys");
                    check(sys::napi_set_named_property(env, out, key.as_ptr(), node(env, x)?))?;
                }
            }
        }
    }
    Ok(out)
}

/// A list of pages.
pub struct JsPages(Vec<Node>);

impl TypeName for JsPages {
    fn type_name() -> &'static str {
        "Array<Page>"
    }
    fn value_type() -> ValueType {
        ValueType::Object
    }
}

impl ToNapiValue for JsPages {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        unsafe { node(env, &Node::List(val.0)) }
    }
}

// ---------------------------------------------------------------------------------------------
// The work (on the big-stack thread)

fn do_extract_text(src: Source, o: Options) -> Result<PyStr> {
    on_big_stack(move || {
        extract_text(&src.file_or_name(), &o.password, o.page_numbers, o.maxpages, o.caching, &o.codec, o.laparams)
    })?
    .map_err(py_error)
}

fn do_extract_pages(src: Source, o: Options) -> Result<Vec<Node>> {
    on_big_stack(move || -> std::result::Result<Vec<Node>, PyErr> {
        let pages = extract_pages(src.file_or_name(), &o.password, o.page_numbers, o.maxpages, o.caching, o.laparams);
        pages.map(|p| p.and_then(|p| pdfminer::dump::item(&p))).collect()
    })?
    .map_err(py_error)
}

fn do_convert(src: Source, o: Options) -> Result<Vec<u8>> {
    on_big_stack(move || -> std::result::Result<Vec<u8>, PyErr> {
        let data = src.data()?;
        let out = Rc::new(std::cell::RefCell::new(BytesOut::new()));
        let args = ExtractTextToFp {
            output_type: o.output_type,
            codec: o.codec,
            laparams: o.laparams,
            maxpages: o.maxpages,
            page_numbers: o.page_numbers,
            password: o.password,
            scale: o.scale,
            rotation: o.rotation,
            layoutmode: o.layoutmode,
            output_dir: o.output_dir,
            strip_control: o.strip_control,
            debug: false,
            disable_caching: !o.caching,
        };
        extract_text_to_fp(data, out.clone(), None, args)?;
        let b = out.borrow_mut().take();
        Ok(b)
    })?
    .map_err(py_error)
}

fn do_dump_layout(src: Source, o: Options) -> Result<String> {
    on_big_stack(move || {
        let mut text = String::new();
        let data = match src.data() {
            Ok(d) => d,
            Err(e) => return pdfminer::dump::status_line(&Err(e)) + "\n",
        };
        let _ = pdfminer::dump::dump_layout(data, o.laparams, &o.password, &mut |s: &str| {
            text.push_str(s);
            Ok(())
        });
        text
    })
}

// ---------------------------------------------------------------------------------------------
// Exports

/// `extract_text(pdf_file, password="", page_numbers=None, maxpages=0, caching=True,
/// codec="utf-8", laparams=None)`: `laparams` defaults to `LAParams()`.
#[napi(ts_args_type = "pdf: Buffer | string, options?: ExtractTextOptions", ts_return_type = "string")]
pub fn extract_text_native(pdf: Either<Buffer, String>, options: Option<Object>) -> Result<JsText> {
    let o = self::options(options, Some(LAParams::default()))?;
    do_extract_text(Source::from_js(pdf), o).map(JsText)
}

/// `extract_pages(...)`: every page, in the format of `tools/oracle.py`.
#[napi(ts_args_type = "pdf: Buffer | string, options?: ExtractPagesOptions", ts_return_type = "Array<Page>")]
pub fn extract_pages_native(pdf: Either<Buffer, String>, options: Option<Object>) -> Result<JsPages> {
    let o = self::options(options, Some(LAParams::default()))?;
    do_extract_pages(Source::from_js(pdf), o).map(JsPages)
}

/// `extract_text_to_fp(inf, outfp=BytesIO(), ...)`: the bytes written (`laparams` defaults to
/// `None`, as in Python).
#[napi(ts_args_type = "pdf: Buffer | string, options?: ConvertOptions")]
pub fn convert_native(pdf: Either<Buffer, String>, options: Option<Object>) -> Result<Buffer> {
    let o = self::options(options, None)?;
    do_convert(Source::from_js(pdf), o).map(Buffer::from)
}

/// The canonical layout dump (`pdfminer-rs dump`): one JSON line per page, then a status line.
#[napi(ts_args_type = "pdf: Buffer | string, options?: DumpLayoutOptions")]
pub fn dump_layout_native(pdf: Either<Buffer, String>, options: Option<Object>) -> Result<String> {
    let o = self::options(options, Some(LAParams::default()))?;
    do_dump_layout(Source::from_js(pdf), o)
}

/// The asynchronous functions: the work runs on libuv's thread pool.
pub struct Work {
    kind: Kind,
    src: Option<Source>,
    options: Options,
}

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Pages,
    Convert,
    Dump,
}

pub enum Output {
    Text(PyStr),
    Pages(Vec<Node>),
    Bytes(Vec<u8>),
    Dump(String),
}

pub struct JsOutput(Output);

impl TypeName for JsOutput {
    fn type_name() -> &'static str {
        "unknown"
    }
    fn value_type() -> ValueType {
        ValueType::Unknown
    }
}

impl ToNapiValue for JsOutput {
    unsafe fn to_napi_value(env: sys::napi_env, val: Self) -> Result<sys::napi_value> {
        unsafe {
            match val.0 {
                Output::Text(s) => utf16(env, &s),
                Output::Pages(p) => node(env, &Node::List(p)),
                Output::Bytes(b) => Buffer::to_napi_value(env, Buffer::from(b)),
                Output::Dump(s) => String::to_napi_value(env, s),
            }
        }
    }
}

impl Task for Work {
    type Output = Output;
    type JsValue = JsOutput;

    fn compute(&mut self) -> Result<Output> {
        let src = self.src.take().expect("computed once");
        let o = self.options.clone();
        match self.kind {
            Kind::Text => do_extract_text(src, o).map(Output::Text),
            Kind::Pages => do_extract_pages(src, o).map(Output::Pages),
            Kind::Convert => do_convert(src, o).map(Output::Bytes),
            Kind::Dump => do_dump_layout(src, o).map(Output::Dump),
        }
    }

    fn resolve(&mut self, _env: Env, output: Output) -> Result<JsOutput> {
        Ok(JsOutput(output))
    }
}

fn task(
    kind: Kind,
    pdf: Either<Buffer, String>,
    options: Option<Object>,
    laparams: Option<LAParams>,
) -> Result<AsyncTask<Work>> {
    let options = self::options(options, laparams)?;
    Ok(AsyncTask::new(Work { kind, src: Some(Source::from_js(pdf)), options }))
}

#[napi(ts_args_type = "pdf: Buffer | string, options?: ExtractTextOptions", ts_return_type = "Promise<string>")]
pub fn extract_text_async_native(pdf: Either<Buffer, String>, options: Option<Object>) -> Result<AsyncTask<Work>> {
    task(Kind::Text, pdf, options, Some(LAParams::default()))
}

#[napi(ts_args_type = "pdf: Buffer | string, options?: ExtractPagesOptions", ts_return_type = "Promise<Array<Page>>")]
pub fn extract_pages_async_native(pdf: Either<Buffer, String>, options: Option<Object>) -> Result<AsyncTask<Work>> {
    task(Kind::Pages, pdf, options, Some(LAParams::default()))
}

#[napi(ts_args_type = "pdf: Buffer | string, options?: ConvertOptions", ts_return_type = "Promise<Buffer>")]
pub fn convert_async_native(pdf: Either<Buffer, String>, options: Option<Object>) -> Result<AsyncTask<Work>> {
    task(Kind::Convert, pdf, options, None)
}

#[napi(ts_args_type = "pdf: Buffer | string, options?: DumpLayoutOptions", ts_return_type = "Promise<string>")]
pub fn dump_layout_async_native(pdf: Either<Buffer, String>, options: Option<Object>) -> Result<AsyncTask<Work>> {
    task(Kind::Dump, pdf, options, Some(LAParams::default()))
}

/// The pdfminer.six release the port follows.
#[napi]
pub fn upstream_version() -> String {
    pdfminer::UPSTREAM_VERSION.to_string()
}
