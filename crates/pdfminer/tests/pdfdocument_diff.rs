//! `PDFDocument` and `PDFPage` against Python on every upstream sample and broken variants of
//! each (reference data from `tools/gen_testdata/pdfdocument.py`).

use std::rc::Rc;

use pdfminer::pdfdocument::PDFDocument;
use pdfminer::pdfpage::PDFPage;
use pdfminer::pdfparser::PDFParser;
use pdfminer::pdftypes::{LITERALS_FLATE_DECODE, in_literals};
use pdfminer::pyrt::error::PyErr;
use pdfminer::pyrt::fmt::float_repr;
use pdfminer::pyrt::io::BytesIO;
use pdfminer::pyrt::value::{Value, dict_repr};
use serde_json::{Value as J, json};
use sha2::{Digest, Sha256};

const MAX_REPR: usize = 300;
const MAX_OUTLINES: usize = 300;

fn samples() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../upstream/pdfminer.six/samples")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02x}")).collect()
}

fn short(s: String) -> String {
    if s.chars().count() <= MAX_REPR {
        return s;
    }
    format!("sha256:{}", hex(&Sha256::digest(s.as_bytes())))
}

fn exc(e: &PyErr) -> String {
    format!("!{}", e.kind.name())
}

fn variants(data: &[u8]) -> Vec<(String, Vec<u8>)> {
    let replace = |hay: &[u8]| -> Vec<u8> {
        let mut out = Vec::with_capacity(hay.len());
        let mut i = 0;
        while i < hay.len() {
            if hay[i..].starts_with(b"startxref") {
                out.extend_from_slice(b"startxreX");
                i += 9;
            } else {
                out.push(hay[i]);
                i += 1;
            }
        }
        out
    };
    let nl = data.iter().position(|&c| c == b'\n').map_or(0, |p| p + 1);
    let mut shifted = data[..nl].to_vec();
    shifted.extend_from_slice(b"%junk\n");
    shifted.extend_from_slice(&data[nl..]);
    let mut v = vec![
        ("orig".to_string(), data.to_vec()),
        ("half".to_string(), data[..data.len() / 2].to_vec()),
        ("3q".to_string(), data[..data.len() * 3 / 4].to_vec()),
        ("noxref".to_string(), replace(data)),
        ("shifted".to_string(), shifted),
        ("tail".to_string(), data[..data.len().saturating_sub(40)].to_vec()),
    ];
    // Random bytes overwritten (the generator's LCG).
    let mut state = data.len() as u64;
    let step = |s: u64| s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    for k in 0..4 {
        let mut b = data.to_vec();
        for _ in 0..6 {
            state = step(state);
            let pos = ((state >> 33) % (b.len().max(1) as u64)) as usize;
            state = step(state);
            if !b.is_empty() {
                b[pos] = ((state >> 33) & 0xFF) as u8;
            }
        }
        v.push((format!("corrupt{k}"), b));
    }
    v
}

fn opt_repr(v: Option<i64>) -> String {
    v.map_or("None".to_string(), |x| x.to_string())
}

fn rect_repr(r: (f64, f64, f64, f64)) -> String {
    format!("({}, {}, {}, {})", float_repr(r.0), float_repr(r.1), float_repr(r.2), float_repr(r.3))
}

fn dump_doc(data: Vec<u8>, password: &str) -> J {
    let parser = PDFParser::new(BytesIO::file(Rc::from(data)));
    let doc = match PDFDocument::new(parser, password, true, true) {
        Ok(d) => d,
        Err(e) => return json!({"open": exc(&e)}),
    };
    let mut out = serde_json::Map::new();
    out.insert("open".into(), json!("ok"));
    let mut xrefs = Vec::new();
    let mut objids: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for xref in doc.xrefs() {
        let mut x = serde_json::Map::new();
        x.insert("repr".into(), json!(short(xref.repr())));
        x.insert("trailer".into(), json!(short(dict_repr(&xref.get_trailer().borrow()))));
        let mut ids = Vec::new();
        for objid in xref.get_objids() {
            match objid {
                Ok(objid) => {
                    let pos = match xref.get_pos(&objid) {
                        Ok((a, b, c)) => format!("({}, {}, {})", opt_repr(a), b.repr(), c),
                        Err(e) => exc(&e),
                    };
                    ids.push(json!([objid.repr(), pos]));
                    if seen.insert(objid.repr()) {
                        objids.push(objid);
                    }
                }
                Err(e) => {
                    x.insert("ids_error".into(), json!(exc(&e)));
                }
            }
        }
        x.insert("ids".into(), J::Array(ids));
        xrefs.push(J::Object(x));
    }
    out.insert("xrefs".into(), J::Array(xrefs));
    out.insert("catalog".into(), json!(short(dict_repr(&doc.catalog().borrow()))));
    let info: Vec<String> = doc.info().iter().map(|d| dict_repr(&d.borrow())).collect();
    out.insert("info".into(), json!(short(format!("[{}]", info.join(", ")))));
    out.insert("flags".into(), json!([doc.is_printable(), doc.is_modifiable(), doc.is_extractable()]));

    let mut objs = Vec::new();
    for objid in &objids {
        let obj = match doc.getobj_value(objid) {
            Ok(o) => o,
            Err(e) => {
                objs.push(json!([objid.repr(), exc(&e), null]));
                continue;
            }
        };
        let r = short(obj.repr());
        let mut data_hash = J::Null;
        if let Value::Stream(s) = &obj {
            match s.get_filters() {
                Ok(filters) => {
                    if filters.iter().all(|(f, _)| in_literals(f, LITERALS_FLATE_DECODE)) {
                        data_hash = match s.get_data() {
                            Ok(d) => json!(hex(&Sha256::digest(&d))),
                            Err(e) => json!(exc(&e)),
                        };
                    }
                }
                Err(e) => data_hash = json!(format!("filters{}", exc(&e))),
            }
        }
        objs.push(json!([objid.repr(), r, data_hash]));
    }
    out.insert("objects".into(), J::Array(objs));

    let mut pages = Vec::new();
    for page in PDFPage::create_pages(&doc) {
        match page {
            Ok(page) => {
                let resources = match &page.resources {
                    Value::Dict(d) => {
                        let mut keys: Vec<String> = d.borrow().keys().map(|k| k.to_string()).collect();
                        keys.sort();
                        json!(keys)
                    }
                    other => json!(format!("type:{}", other.type_name())),
                };
                let contents: Vec<String> = page.contents.iter().map(|c| short(c.repr())).collect();
                let attrs: Vec<String> = page.attrs.borrow().keys().map(|k| k.to_string()).collect();
                pages.push(json!({
                    "pageid": page.pageid.repr(),
                    "label": page.label,
                    "mediabox": rect_repr(page.mediabox),
                    "cropbox": rect_repr(page.cropbox),
                    "rotate": page.rotate,
                    "resources": resources,
                    "contents": contents,
                    "attrs": attrs,
                    "repr": short(page.repr()),
                }));
            }
            Err(e) => pages.push(json!(exc(&e))),
        }
    }
    out.insert("pages".into(), J::Array(pages));

    let mut outlines = Vec::new();
    match doc.get_outlines() {
        Ok(it) => {
            for o in it.take(MAX_OUTLINES) {
                match o {
                    Ok(o) => outlines.push(json!([
                        o.level,
                        o.title,
                        short(o.dest.repr()),
                        short(o.action.repr()),
                        short(o.se.repr())
                    ])),
                    Err(e) => outlines.push(json!(exc(&e))),
                }
            }
        }
        Err(e) => outlines.push(json!(exc(&e))),
    }
    out.insert("outlines".into(), J::Array(outlines));

    let mut labels = Vec::new();
    match doc.get_page_labels() {
        Ok(it) => {
            for l in it.take(12) {
                match l {
                    Ok(l) => labels.push(json!(l)),
                    Err(e) => labels.push(json!(exc(&e))),
                }
            }
        }
        Err(e) => labels.push(json!(exc(&e))),
    }
    out.insert("labels".into(), J::Array(labels));

    let mut dests = Vec::new();
    for name in [
        Value::bytes(b"foo"),
        Value::str("foo"),
        Value::bytes(b"Chapter1"),
        Value::str("Chapter1"),
        Value::bytes(b"zzz"),
    ] {
        match doc.get_dest(&name) {
            Ok(v) => dests.push(json!(short(v.repr()))),
            Err(e) => dests.push(json!(exc(&e))),
        }
    }
    out.insert("dests".into(), J::Array(dests));
    J::Object(out)
}

/// The first difference between two JSON values, with its path.
fn first_diff(path: &str, want: &J, got: &J) -> Option<String> {
    match (want, got) {
        (J::Object(a), J::Object(b)) => {
            for (k, v) in a {
                let p = format!("{path}.{k}");
                match b.get(k) {
                    Some(w) => {
                        if let Some(d) = first_diff(&p, v, w) {
                            return Some(d);
                        }
                    }
                    None => return Some(format!("{p}: missing (want {v})")),
                }
            }
            for k in b.keys() {
                if !a.contains_key(k) {
                    return Some(format!("{path}.{k}: unexpected {}", b[k]));
                }
            }
            None
        }
        (J::Array(a), J::Array(b)) => {
            for (i, (v, w)) in a.iter().zip(b).enumerate() {
                if let Some(d) = first_diff(&format!("{path}[{i}]"), v, w) {
                    return Some(d);
                }
            }
            if a.len() != b.len() {
                return Some(format!(
                    "{path}: length {} != {} (want tail {:?}, got tail {:?})",
                    a.len(),
                    b.len(),
                    a.get(b.len()),
                    b.get(a.len())
                ));
            }
            None
        }
        _ if want == got => None,
        _ => Some(format!("{path}: want {want}, got {got}")),
    }
}

#[test]
fn pdfdocument_matches_python() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pdfdocument");
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    assert!(files.len() >= 50, "reference data missing");
    let filter = std::env::var("PDFDOC_FILTER").ok();
    let mut failures = Vec::new();
    let mut runs = 0;
    for f in files {
        let want: J = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        let rel = want["path"].as_str().unwrap();
        if filter.as_ref().is_some_and(|flt| !rel.contains(flt.as_str())) {
            continue;
        }
        let data = match rel.strip_prefix("synth/") {
            Some(name) => std::fs::read(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pdfdocument_synth").join(name),
            )
            .unwrap(),
            None => std::fs::read(samples().join(rel)).unwrap(),
        };
        let mut i = 0;
        let runs_want = want["runs"].as_array().unwrap();
        let passwords: Vec<String> = {
            let mut p: Vec<String> = Vec::new();
            for r in runs_want {
                let pw = r["password"].as_str().unwrap().to_string();
                if !p.contains(&pw) {
                    p.push(pw);
                }
            }
            p
        };
        for pw in &passwords {
            for (vname, vdata) in variants(&data) {
                let w = &runs_want[i];
                i += 1;
                assert_eq!(w["variant"], vname);
                let mut got = dump_doc(vdata, pw);
                let obj = got.as_object_mut().unwrap();
                obj.insert("password".into(), json!(pw));
                obj.insert("variant".into(), json!(vname));
                runs += 1;
                if let Some(d) = first_diff("", w, &got) {
                    failures.push(format!("{rel} [{vname}, password={pw:?}]: {d}"));
                }
            }
        }
        assert_eq!(i, runs_want.len());
    }
    eprintln!("compared {runs} runs");
    assert!(failures.is_empty(), "{} of {runs} runs differ:\n{}", failures.len(), failures.join("\n"));
}
