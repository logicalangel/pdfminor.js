//! Port of `tools/dumppdf.py`: extract pdf structure in XML format.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::rc::Rc;

use pdfminer::high_level::read_file;
use pdfminer::layout::PY_RECURSION_LIMIT;
use pdfminer::pdfdocument::{ObjKey, PDFBaseXRef, PDFDocument, obj_key};
use pdfminer::pdfpage::PDFPage;
use pdfminer::pdfparser::PDFParser;
use pdfminer::pdftypes::{resolve1, stream_value};
use pdfminer::psparser::NameType;
use pdfminer::pyrt::PyStr;
use pdfminer::pyrt::error::{ExcKind, PyErr, PyResult};
use pdfminer::pyrt::fmt::{bytes_repr, float_repr, str_repr};
use pdfminer::pyrt::io::BytesIO;
use pdfminer::pyrt::textio::{Codec, Errors, PyFile, TextFile, os_error, os_error_path, stdout_encoding};
use pdfminer::pyrt::value::{Value, py_int};

use crate::argparse::{ArgVal, ArgumentParser, Kind, Namespace, Nargs};
use crate::pdf2txt::{conv_int, get_bool, get_str, get_strs, page_numbers_arg, pagenos_arg};
use crate::{Stdout, die};

const DOC: &str = "Extract pdf structure in XML format";

/// The `out` of the dump functions: a text stream.
type Out<'a> = &'a mut dyn PyFile;

fn write(out: Out<'_>, s: &str) -> PyResult<()> {
    out.write_str(&PyStr::from(s))
}

/// `print(s)` (to `sys.stdout`).
fn print(stdout: &mut dyn PyFile, s: &str) -> PyResult<()> {
    stdout.write_str(&PyStr::from(format!("{s}\n")))
}

fn is_escaped(c: char) -> bool {
    let o = c as u32;
    o <= 0o37 || "&<>()\"'\\".contains(c) || (0o177..=0o377).contains(&o)
}

/// `escape(s)`: `ESC_PAT = r'[\000-\037&<>()"\042\047\134\177-\377]'` -> `&#{ord};`.
fn escape_chars(s: impl Iterator<Item = char>) -> String {
    let mut out = String::new();
    for c in s {
        if is_escaped(c) {
            out.push_str(&format!("&#{};", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// `ESC_PAT.sub` calls its lambda (two more frames) only when something matches.
fn needs_escape(mut s: impl Iterator<Item = char>) -> bool {
    s.any(is_escaped)
}

fn escape_bytes(b: &[u8]) -> String {
    escape_chars(b.iter().map(|&c| c as char))
}

fn escape_str(s: &str) -> String {
    escape_chars(s.chars())
}

/// `RecursionError` when a Python frame at `depth` would exceed the limit.
fn enter(depth: usize) -> PyResult<()> {
    if depth > PY_RECURSION_LIMIT {
        return Err(PyErr::new(ExcKind::RecursionError, "maximum recursion depth exceeded"));
    }
    Ok(())
}

/// `dumpxml(out, obj, codec)`, as a frame at `depth`.
fn dumpxml(out: Out<'_>, obj: &Value, codec: Option<&str>, depth: usize) -> PyResult<()> {
    enter(depth)?;
    match obj {
        Value::None => write(out, "<null />"),
        Value::Dict(d) => {
            let items: Vec<(Rc<str>, Value)> = d.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            write(out, &format!("<dict size=\"{}\">\n", items.len()))?;
            for (k, v) in items {
                write(out, &format!("<key>{k}</key>\n"))?;
                write(out, "<value>")?;
                dumpxml(out, &v, None, depth + 1)?;
                write(out, "</value>\n")?;
            }
            write(out, "</dict>")
        }
        Value::List(l) => {
            let items = l.borrow().clone();
            write(out, &format!("<list size=\"{}\">\n", items.len()))?;
            for v in items {
                dumpxml(out, &v, None, depth + 1)?;
                write(out, "\n")?;
            }
            write(out, "</list>")
        }
        Value::Str(s) => {
            if needs_escape(s.chars()) {
                enter(depth + 2)?;
            }
            write(out, &format!("<string size=\"{}\">{}</string>", s.chars().count(), escape_str(s)))
        }
        Value::Bytes(b) => {
            if needs_escape(b.iter().map(|&c| c as char)) {
                enter(depth + 2)?;
            }
            write(out, &format!("<string size=\"{}\">{}</string>", b.len(), escape_bytes(b)))
        }
        Value::Stream(s) => {
            if codec == Some("raw") {
                // Bug: writing bytes to text I/O. This will raise TypeError.
                out.write_bytes(&s.get_rawdata().unwrap_or_else(|| Rc::from(&b""[..])))
            } else if codec == Some("binary") {
                // Bug: writing bytes to text I/O. This will raise TypeError.
                let data = s.get_data()?;
                out.write_bytes(&data)
            } else {
                write(out, "<stream>\n<props>\n")?;
                dumpxml(out, &Value::Dict(s.attrs.clone()), None, depth + 1)?;
                write(out, "\n</props>\n")?;
                if codec == Some("text") {
                    let data = s.get_data()?;
                    if needs_escape(data.iter().map(|&c| c as char)) {
                        enter(depth + 2)?;
                    }
                    write(out, &format!("<data size=\"{}\">{}</data>\n", data.len(), escape_bytes(&data)))?;
                }
                write(out, "</stream>")
            }
        }
        Value::Ref(r) => write(out, &format!("<ref id=\"{}\" />", r.objid_value().repr())),
        Value::Keyword(k) => {
            // Likely bug: obj.name is bytes, not str
            write(out, &format!("<keyword>{}</keyword>", bytes_repr(&k.name)))
        }
        Value::Literal(l) => {
            // Likely bug: obj.name may be bytes, not str
            let name = match &l.name {
                NameType::Str(s) => s.to_string(),
                NameType::Bytes(b) => bytes_repr(b),
            };
            write(out, &format!("<literal>{name}</literal>"))
        }
        Value::Int(i) => write(out, &format!("<number>{i}</number>")),
        Value::BigInt(i) => write(out, &format!("<number>{i}</number>")),
        Value::Float(f) => write(out, &format!("<number>{}</number>", float_repr(*f))),
        Value::Bool(b) => write(out, &format!("<number>{}</number>", if *b { "True" } else { "False" })),
        Value::Tuple(_) => Err(PyErr::new(ExcKind::PDFTypeError, obj.py_str())),
    }
}

fn dumptrailers(out: Out<'_>, doc: &PDFDocument, show_fallback_xref: bool, depth: usize) -> PyResult<()> {
    let xrefs = doc.xrefs();
    for xref in &xrefs {
        if !matches!(**xref, PDFBaseXRef::Fallback(_)) || show_fallback_xref {
            write(out, "<trailer>\n")?;
            dumpxml(out, &Value::Dict(xref.get_trailer()), None, depth + 1)?;
            write(out, "\n</trailer>\n\n")?;
        }
    }
    let no_xrefs = xrefs.iter().all(|x| matches!(**x, PDFBaseXRef::Fallback(_)));
    if no_xrefs && !show_fallback_xref {
        let msg = "This PDF does not have an xref. Use --show-fallback-xref if you want to display the content of a \
                   fallback xref that contains all objects.";
        log::warn!("{msg}");
    }
    Ok(())
}

fn dumpallobjs(
    out: Out<'_>,
    stdout: &mut dyn PyFile,
    out_is_stdout: bool,
    doc: &PDFDocument,
    codec: Option<&str>,
    show_fallback_xref: bool,
) -> PyResult<()> {
    let mut visited: HashSet<ObjKey> = HashSet::new();
    write(out, "<pdf>")?;
    for xref in doc.xrefs() {
        for objid in xref.get_objids() {
            let objid = objid?;
            let key = obj_key(&objid)?;
            if visited.contains(&key) {
                continue;
            }
            visited.insert(key);
            match doc.getobj_value(&objid) {
                Ok(obj) => {
                    if obj.is_none() {
                        continue;
                    }
                    write(out, &format!("<object id=\"{}\">\n", objid.py_str()))?;
                    dumpxml(out, &obj, codec, 5)?;
                    write(out, "\n</object>\n\n")?;
                }
                Err(e) if e.is(ExcKind::PDFObjectNotFound) => {
                    // print(f"not found: {e!r}") goes to sys.stdout.
                    let line = format!("not found: {}({})", e.kind.name(), e.msg);
                    if out_is_stdout {
                        print(out, &line)?;
                    } else {
                        print(stdout, &line)?;
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
    dumptrailers(out, doc, show_fallback_xref, 5)?;
    write(out, "</pdf>")
}

/// `obj[0]` of a destination.
fn getitem0(v: &Value) -> PyResult<Value> {
    match v {
        Value::List(l) => {
            l.borrow().first().cloned().ok_or_else(|| PyErr::new(ExcKind::IndexError, "list index out of range"))
        }
        Value::Tuple(t) => {
            t.first().cloned().ok_or_else(|| PyErr::new(ExcKind::IndexError, "tuple index out of range"))
        }
        Value::Bytes(b) => b
            .first()
            .map(|&c| Value::Int(c as i64))
            .ok_or_else(|| PyErr::new(ExcKind::IndexError, "index out of range")),
        Value::Str(s) => s
            .chars()
            .next()
            .map(|c| Value::str(&c.to_string()))
            .ok_or_else(|| PyErr::new(ExcKind::IndexError, "string index out of range")),
        Value::Dict(_) => Err(PyErr::new(ExcKind::KeyError, "0")),
        Value::Stream(s) => Err(s.getitem("0").unwrap_err()),
        other => Err(PyErr::new(ExcKind::TypeError, format!("'{}' object is not subscriptable", other.type_name()))),
    }
}

/// `x.objid`.
fn objid_of(v: &Value) -> PyResult<Value> {
    match v {
        Value::Ref(r) => Ok(r.objid_value()),
        Value::Stream(s) => Ok(s.objid.get().map_or(Value::None, Value::Int)),
        other => {
            Err(PyErr::new(ExcKind::AttributeError, format!("'{}' object has no attribute 'objid'", other.type_name())))
        }
    }
}

fn open_document(fname: &str, password: &str) -> PyResult<Rc<PDFDocument>> {
    let data = read_file(fname)?;
    let parser = PDFParser::new(BytesIO::file(data));
    PDFDocument::new(parser, password, true, true)
}

fn dumpoutline(out: Out<'_>, fname: &str, password: &str) -> PyResult<()> {
    let doc = open_document(fname, password)?;
    let mut pages: HashMap<ObjKey, i64> = HashMap::new();
    for (pageno, page) in PDFPage::create_pages(&doc).enumerate() {
        let page = page?;
        pages.insert(obj_key(&page.pageid)?, pageno as i64 + 1);
    }

    let resolve_dest = |dest: &Value| -> PyResult<Value> {
        let mut dest = dest.clone();
        match &dest {
            Value::Str(_) | Value::Bytes(_) => dest = resolve1(&doc.get_dest(&dest)?)?,
            Value::Literal(l) => {
                let name = match &l.name {
                    NameType::Str(s) => Value::Str(s.clone()),
                    NameType::Bytes(b) => Value::Bytes(b.clone()),
                };
                dest = resolve1(&doc.get_dest(&name)?)?;
            }
            _ => {}
        }
        if let Value::Dict(d) = &dest {
            let v = d.borrow().get("D").cloned();
            dest = v.ok_or_else(|| PyErr::new(ExcKind::KeyError, "'D'"))?;
        }
        if let Value::Ref(r) = &dest {
            dest = r.resolve(&Value::None)?;
        }
        Ok(dest)
    };
    let lookup_page = |dest: &Value| -> PyResult<i64> {
        let objid = objid_of(&getitem0(dest)?)?;
        pages.get(&obj_key(&objid)?).copied().ok_or_else(|| PyErr::new(ExcKind::KeyError, objid.repr()))
    };

    let r = (|| -> PyResult<()> {
        let outlines = doc.get_outlines()?;
        write(out, "<outlines>\n")?;
        for o in outlines {
            let o = o?;
            let mut dest = o.dest.clone();
            let mut pageno = None;
            if dest.truthy() {
                dest = resolve_dest(&dest)?;
                pageno = Some(lookup_page(&dest)?);
            } else if o.action.truthy()
                && let Value::Dict(action) = &o.action
            {
                let subtype = action.borrow().get("S").cloned().unwrap_or(Value::None);
                let d = action.borrow().get("D").cloned().unwrap_or(Value::None);
                if subtype.truthy() && subtype.repr() == "/'GoTo'" && d.truthy() {
                    dest = resolve_dest(&d)?;
                    pageno = Some(lookup_page(&dest)?);
                }
            }
            let s = escape_str(&o.title);
            write(out, &format!("<outline level=\"{}\" title=\"{}\">\n", o.level, s))?;
            if !dest.is_none() {
                write(out, "<dest>")?;
                dumpxml(out, &dest, None, 4)?;
                write(out, "</dest>\n")?;
            }
            if let Some(p) = pageno {
                write(out, &format!("<pageno>{p}</pageno>\n"))?;
            }
            write(out, "</outline>\n")?;
        }
        write(out, "</outlines>\n")
    })();
    match r {
        Err(e) if e.is(ExcKind::PDFNoOutlines) => Ok(()),
        other => other,
    }
}

/// `os.path.basename(p)`.
fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn extractembedded(stdout: &mut dyn PyFile, fname: &str, password: &str, extractdir: &str) -> PyResult<()> {
    let doc = open_document(fname, password)?;

    let extract1 = |stdout: &mut dyn PyFile, objid: &Value, obj: &pdfminer::pyrt::DictRef| -> PyResult<()> {
        let get = |k: &str| obj.borrow().get(k).cloned().unwrap_or(Value::None);
        let uf = get("UF");
        // filename = os.path.basename(obj.get("UF") or cast(bytes, obj.get("F")).decode())
        let (filename, filename_repr) = if uf.truthy() {
            match &uf {
                Value::Str(s) => (basename(s).to_string(), str_repr(basename(s))),
                Value::Bytes(b) => {
                    let base = match b.iter().rposition(|&c| c == b'/') {
                        Some(i) => &b[i + 1..],
                        None => &b[..],
                    };
                    (bytes_repr(base), bytes_repr(base))
                }
                other => {
                    return Err(PyErr::new(
                        ExcKind::TypeError,
                        format!("expected str, bytes or os.PathLike object, not {}", other.type_name()),
                    ));
                }
            }
        } else {
            match get("F") {
                Value::Bytes(b) => match std::str::from_utf8(&b) {
                    Ok(s) => (basename(s).to_string(), str_repr(basename(s))),
                    Err(e) => {
                        let pos = e.valid_up_to();
                        let msg = format!(
                            "'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte",
                            b[pos], pos
                        );
                        return Err(PyErr::new(ExcKind::UnicodeDecodeError, msg));
                    }
                },
                other => {
                    return Err(PyErr::new(
                        ExcKind::AttributeError,
                        format!("'{}' object has no attribute 'decode'", other.type_name()),
                    ));
                }
            }
        };
        let ef = obj.borrow().get("EF").cloned().ok_or_else(|| PyErr::new(ExcKind::KeyError, "'EF'"))?;
        let Value::Dict(ef) = &ef else {
            return Err(PyErr::new(
                ExcKind::AttributeError,
                format!("'{}' object has no attribute 'get'", ef.type_name()),
            ));
        };
        let mut fileref = ef.borrow().get("UF").cloned().unwrap_or(Value::None);
        if !fileref.truthy() {
            fileref = ef.borrow().get("F").cloned().unwrap_or(Value::None);
        }
        let fileobj = doc.getobj_value(&objid_of(&fileref)?)?;
        let Value::Stream(fileobj) = fileobj else {
            let msg = format!("unable to process PDF: reference for {filename_repr} is not a PDFStream");
            return Err(PyErr::new(ExcKind::PDFValueError, msg));
        };
        let is_embedded = matches!(fileobj.get("Type"), Some(Value::Literal(l)) if l.is("EmbeddedFile"));
        if !is_embedded {
            return Err(PyErr::new(
                ExcKind::PDFValueError,
                format!("unable to process PDF: reference for {filename_repr} is not an EmbeddedFile"),
            ));
        }
        let objid_str = match objid {
            Value::Int(i) if *i < 0 => format!("-{:05}", -(*i as i128)),
            Value::Int(i) => format!("{i:06}"),
            Value::BigInt(i) => i.to_string(), // longer than the width
            other => {
                return Err(PyErr::new(
                    ExcKind::ValueError,
                    format!("Unknown format code 'd' for object of type '{}'", other.type_name()),
                ));
            }
        };
        let name = format!("{objid_str}-{filename}");
        let path = if extractdir.is_empty() || name.starts_with('/') {
            name.clone()
        } else if extractdir.ends_with('/') {
            format!("{extractdir}{name}")
        } else {
            format!("{extractdir}/{name}")
        };
        if std::fs::metadata(&path).is_ok() {
            return Err(PyErr::new(ExcKind::PDFIOError, format!("file exists: {}", str_repr(&path))));
        }
        print(stdout, &format!("extracting: {}", str_repr(&path)))?;
        let dir = match path.rfind('/') {
            Some(i) => &path[..i],
            None => "",
        };
        if !dir.is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| os_error_path(&e, dir))?;
        }
        let data = fileobj.get_data()?;
        std::fs::write(&path, &data[..]).map_err(|e| os_error_path(&e, &path))?;
        Ok(())
    };

    let mut extracted_objids: HashSet<ObjKey> = HashSet::new();
    for xref in doc.xrefs() {
        for objid in xref.get_objids() {
            let objid = objid?;
            let obj = doc.getobj_value(&objid)?;
            let key = obj_key(&objid)?;
            if !extracted_objids.contains(&key)
                && let Value::Dict(d) = &obj
                && matches!(d.borrow().get("Type"), Some(Value::Literal(l)) if l.is("Filespec"))
            {
                extracted_objids.insert(key);
                extract1(stdout, &objid, d)?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn dumppdf(
    out: Out<'_>,
    stdout: &mut dyn PyFile,
    out_is_stdout: bool,
    fname: &str,
    objids: &[i64],
    pagenos: &HashSet<i64>,
    password: &str,
    dumpall: bool,
    codec: Option<&str>,
    show_fallback_xref: bool,
) -> PyResult<()> {
    {
        let doc = open_document(fname, password)?;
        for &objid in objids {
            let obj = doc.getobj(objid)?;
            dumpxml(out, &obj, codec, 4)?;
        }
        if !pagenos.is_empty() {
            for (pageno, page) in PDFPage::create_pages(&doc).enumerate() {
                let page = page?;
                if pagenos.contains(&(pageno as i64)) {
                    if codec.is_some_and(|c| !c.is_empty()) {
                        for obj in &page.contents {
                            let obj = stream_value(obj)?;
                            dumpxml(out, &Value::Stream(obj), codec, 4)?;
                        }
                    } else {
                        dumpxml(out, &Value::Dict(page.attrs.clone()), None, 4)?;
                    }
                }
            }
        }
        if dumpall {
            dumpallobjs(out, stdout, out_is_stdout, &doc, codec, show_fallback_xref)?;
        }
        if objids.is_empty() && pagenos.is_empty() && !dumpall {
            dumptrailers(out, &doc, show_fallback_xref, 4)?;
        }
    }
    if codec != Some("raw") && codec != Some("binary") {
        write(out, "\n")?;
    }
    Ok(())
}

pub fn create_parser() -> ArgumentParser {
    let mut parser = ArgumentParser::new("dumppdf.py", Some(DOC));
    parser.version = format!("pdfminer.six v{}", pdfminer::UPSTREAM_VERSION);
    let p = &mut parser;
    let (store, store_true, one, zero) = (Kind::Store, Kind::StoreTrue, Nargs::One, Nargs::Zero);
    p.add_argument(0, &["files"], store, Nargs::Plus, None, ArgVal::None, "One or more paths to PDF files.");
    p.add_argument(
        1,
        &["--version", "-v"],
        Kind::Version,
        zero,
        None,
        ArgVal::None,
        "show program's version number and exit",
    );
    p.add_argument(1, &["--debug", "-d"], store_true, zero, None, ArgVal::Bool(false), "Use debug logging level.");
    let procedure = p.add_mutually_exclusive_group();
    let a = p.add_argument(
        1,
        &["--extract-toc", "-T"],
        store_true,
        zero,
        None,
        ArgVal::Bool(false),
        "Extract structure of outline",
    );
    p.add_to_mutex(procedure, a);
    let a = p.add_argument(1, &["--extract-embedded", "-E"], store, one, None, ArgVal::None, "Extract embedded files");
    p.add_to_mutex(procedure, a);

    let parse_params = p.add_argument_group("Parser", Some("Used during PDF parsing"));
    p.add_argument(
        parse_params,
        &["--page-numbers"],
        store,
        Nargs::Plus,
        Some((conv_int, "int")),
        ArgVal::None,
        "A space-seperated list of page numbers to parse.",
    );
    p.add_argument(
        parse_params,
        &["--pagenos", "-p"],
        store,
        one,
        None,
        ArgVal::None,
        "A comma-separated list of page numbers to parse. Included for legacy applications, use --page-numbers for more idiomatic argument entry.",
    );
    p.add_argument(
        parse_params,
        &["--objects", "-i"],
        store,
        one,
        None,
        ArgVal::None,
        "Comma separated list of object numbers to extract",
    );
    p.add_argument(
        parse_params,
        &["--all", "-a"],
        store_true,
        zero,
        None,
        ArgVal::Bool(false),
        "If the structure of all objects should be extracted",
    );
    p.add_argument(
        parse_params,
        &["--show-fallback-xref"],
        store_true,
        zero,
        None,
        ArgVal::Bool(false),
        "Additionally show the fallback xref. Use this if the PDF has zero or only invalid xref's. This setting is ignored if --extract-toc or --extract-embedded is used.",
    );
    p.add_argument(
        parse_params,
        &["--password", "-P"],
        store,
        one,
        None,
        ArgVal::Str(String::new()),
        "The password to use for decrypting PDF file.",
    );

    let out = p.add_argument_group("Output", Some("Used during output generation."));
    p.add_argument(
        out,
        &["--outfile", "-o"],
        store,
        one,
        None,
        ArgVal::Str("-".into()),
        "Path to file where output is written. Or \"-\" (default) to write to stdout.",
    );
    let codec_group = p.add_mutually_exclusive_group();
    for (names, help) in [
        (["--raw-stream", "-r"], "Write stream objects without encoding"),
        (["--binary-stream", "-b"], "Write stream objects with binary encoding"),
        (["--text-stream", "-t"], "Write stream objects as plain text"),
    ] {
        let a = p.add_argument(out, &names, store_true, zero, None, ArgVal::Bool(false), help);
        p.add_to_mutex(codec_group, a);
    }
    parser
}

fn run(ns: &Namespace, stdout: &Stdout) -> PyResult<()> {
    let objids: Vec<i64> = match get_str(ns, "objects").filter(|s| !s.is_empty()) {
        Some(s) => s.split(',').map(|x| py_int(&Value::str(x))).collect::<PyResult<_>>()?,
        None => vec![],
    };

    let pagenos: HashSet<i64> = match page_numbers_arg(ns) {
        Some(p) => p,
        None => match get_str(ns, "pagenos").filter(|s| !s.is_empty()) {
            Some(p) => pagenos_arg(&p)?,
            None => HashSet::new(),
        },
    };

    let password = get_str(ns, "password").unwrap_or_default();

    let codec = if get_bool(ns, "raw_stream") {
        Some("raw")
    } else if get_bool(ns, "binary_stream") {
        Some("binary")
    } else if get_bool(ns, "text_stream") {
        Some("text")
    } else {
        None
    };

    let (encoding, errors) = stdout_encoding();
    let mut sys_stdout = TextFile::new(stdout.clone(), encoding, errors);
    let outfile = get_str(ns, "outfile").unwrap_or_default();
    let mut file_out = if outfile == "-" {
        None
    } else {
        let f = std::fs::File::create(&outfile).map_err(|e| os_error_path(&e, &outfile))?;
        // open(outfile, "w"): the locale's encoding (UTF-8), strict.
        Some(TextFile::new(std::io::BufWriter::new(f), Codec::Utf8, Errors::Strict))
    };
    let extract_toc = get_bool(ns, "extract_toc");
    let extract_embedded = get_str(ns, "extract_embedded").filter(|s| !s.is_empty());
    let dumpall = get_bool(ns, "all");
    let show_fallback_xref = get_bool(ns, "show_fallback_xref");
    let files = get_strs(ns, "files");

    let r = (|| -> PyResult<()> {
        for fname in &files {
            let out_is_stdout = file_out.is_none();
            if extract_toc {
                match file_out.as_mut() {
                    Some(f) => dumpoutline(f, fname, &password)?,
                    None => dumpoutline(&mut sys_stdout, fname, &password)?,
                }
            } else if let Some(dir) = &extract_embedded {
                extractembedded(&mut sys_stdout, fname, &password, dir)?;
            } else {
                // `print` goes to sys.stdout: `out` itself when writing to stdout.
                let mut unused = TextFile::new(Vec::new(), encoding, errors);
                let (out, stdout_for_print): (&mut dyn PyFile, &mut dyn PyFile) = match file_out.as_mut() {
                    Some(f) => (f, &mut sys_stdout),
                    None => (&mut sys_stdout, &mut unused),
                };
                dumppdf(
                    out,
                    stdout_for_print,
                    out_is_stdout,
                    fname,
                    &objids,
                    &pagenos,
                    &password,
                    dumpall,
                    codec,
                    show_fallback_xref,
                )?;
            }
        }
        Ok(())
    })();
    let f = match file_out.as_mut() {
        Some(f) => f.inner.flush().map_err(|e| os_error(&e)),
        None => Ok(()),
    };
    r?;
    f
}

pub fn main(args: &[String]) -> i32 {
    let mut stdout = Stdout::new();
    let parser = create_parser();
    let ns = match parser.parse_args(args) {
        Ok(ns) => ns,
        Err(exit) => {
            let _ = stdout.write_all(exit.stdout.as_bytes());
            let _ = stdout.flush();
            let _ = std::io::stderr().write_all(exit.stderr.as_bytes());
            return exit.code;
        }
    };
    if let Err(e) = run(&ns, &stdout) {
        die(&mut stdout, &e);
    }
    let _ = stdout.flush();
    0
}
