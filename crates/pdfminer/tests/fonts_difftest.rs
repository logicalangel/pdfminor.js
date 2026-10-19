//! Differential tests of `cmapdb` and `pdffont` against the reference pdfminer.six.
//!
//! The data in `tests/data/fonts/` is written by `tools/gen_testdata/fonts.py` (see there): every
//! embedded cmap, and every font of the upstream sample PDFs with Python's results. Each font is
//! rebuilt here from its dumped spec and every result is compared exactly (floats by `repr`,
//! exceptions by class name).

use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use pdfminer::cmapdb::{CMapBase, CMapDB, is_cmap_not_found};
use pdfminer::pdffont::{CFFFont, CFFString, PDFFont, TrueTypeFont, is_ttf_cmap_not_found};
use pdfminer::pdftypes::PDFStream;
use pdfminer::psparser::NameType;
use pdfminer::psparser::{KWD, LIT, LIT_bytes};
use pdfminer::pyrt::PyStr;
use pdfminer::pyrt::error::{PyErr, PyResult};
use pdfminer::pyrt::fmt::float_repr;
use pdfminer::pyrt::io::BytesIO;
use pdfminer::pyrt::value::{Dict, Value};
use serde_json::{Value as J, json};
use sha2::{Digest, Sha256};

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/fonts")
}

fn load(path: &Path) -> J {
    let gz = std::fs::read(path).unwrap();
    let mut s = String::new();
    flate2::read::GzDecoder::new(&gz[..]).read_to_string(&mut s).unwrap();
    serde_json::from_str(&s).unwrap()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02x}")).collect()
}

fn cps_string(j: &J) -> String {
    j.as_array().unwrap().iter().map(|c| char::from_u32(c.as_u64().unwrap() as u32).unwrap()).collect()
}

fn sha256(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

/// The class name Python prints for an exception (`!Name`).
fn exc(e: &PyErr) -> String {
    format!("!{}", e.kind.name())
}

/// Decodes the tagged JSON of the generator into a `Value`.
fn dec(j: &J) -> Value {
    match j {
        J::Null => Value::None,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => Value::Int(n.as_i64().unwrap()),
        J::Object(o) => {
            let (tag, v) = o.iter().next().unwrap();
            match tag.as_str() {
                "f" => Value::Float(match v.as_str().unwrap() {
                    "inf" => f64::INFINITY,
                    "-inf" => f64::NEG_INFINITY,
                    "nan" => f64::NAN,
                    s => s.parse().unwrap(),
                }),
                "bytes" => Value::bytes(&unhex(v.as_str().unwrap())),
                "str" => Value::str(&cps_string(v)),
                "lit" => Value::Literal(LIT(&cps_string(v))),
                "litb" => Value::Literal(LIT_bytes(&unhex(v.as_str().unwrap()))),
                "kwd" => Value::Keyword(KWD(&unhex(v.as_str().unwrap()))),
                "list" => Value::list(v.as_array().unwrap().iter().map(dec).collect()),
                "tuple" => Value::tuple(v.as_array().unwrap().iter().map(dec).collect()),
                "dict" => Value::dict(dec_dict(v)),
                "stream" => {
                    let attrs = Rc::new(RefCell::new(dec_dict(&v["attrs"])));
                    let data = unhex(v["data"].as_str().unwrap());
                    Value::Stream(Rc::new(PDFStream::new(attrs, Rc::from(data), None)))
                }
                t => panic!("unknown tag {t}"),
            }
        }
        _ => panic!("bad value {j}"),
    }
}

fn dec_dict(j: &J) -> Dict {
    let mut d = Dict::new();
    for kv in j.as_array().unwrap() {
        d.insert(Rc::from(kv[0].as_str().unwrap()), dec(&kv[1]));
    }
    d
}

fn pystr_cps(s: &PyStr) -> J {
    J::Array(s.code_points().map(|c| json!(c)).collect())
}

/// The tagged JSON of a `Value` (the generator's `enc`).
fn enc(v: &Value) -> J {
    match v {
        Value::None => J::Null,
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!({"f": float_repr(*f)}),
        Value::Bytes(b) => json!({"bytes": hex(b)}),
        Value::Str(s) => json!({"str": s.chars().map(|c| c as u32).collect::<Vec<_>>()}),
        Value::Literal(l) => match &l.name {
            NameType::Str(s) => json!({"lit": s.chars().map(|c| c as u32).collect::<Vec<_>>()}),
            NameType::Bytes(b) => json!({"litb": hex(b)}),
        },
        Value::Keyword(k) => json!({"kwd": hex(&k.name)}),
        Value::List(l) => json!({"list": l.borrow().iter().map(enc).collect::<Vec<_>>()}),
        Value::Tuple(t) => json!({"tuple": t.iter().map(enc).collect::<Vec<_>>()}),
        Value::Dict(d) => {
            json!({"dict": d.borrow().iter().map(|(k, v)| json!([k.to_string(), enc(v)])).collect::<Vec<_>>()})
        }
        Value::Stream(s) => json!({"stream": {
            "attrs": s.attrs.borrow().iter().map(|(k, v)| json!([k.to_string(), enc(v)])).collect::<Vec<_>>(),
            // Like the generator: the raw data, without decoding the stream.
            "data": hex(&s.data.borrow().clone().or_else(|| s.get_rawdata()).unwrap()),
        }}),
        other => panic!("cannot encode {other:?}"),
    }
}

fn res<T>(r: PyResult<T>, f: impl FnOnce(T) -> J) -> J {
    match r {
        Ok(v) => f(v),
        Err(e) => json!(exc(&e)),
    }
}

/// Collects mismatches instead of stopping at the first.
struct Diffs(Vec<String>);

impl Diffs {
    fn check(&mut self, what: impl FnOnce() -> String, got: J, want: &J) {
        if &got != want {
            if self.0.len() < 200 {
                self.0.push(format!("{}: got {got}, want {want}", what()));
            } else if self.0.len() == 200 {
                self.0.push("...".into());
            }
        }
    }
}

fn cmap_dump_hash(m: &CMapBase) -> String {
    sha256(&m.dump().unwrap())
}

#[test]
fn cmaps_match_python() {
    let data = load(&data_dir().join("cmaps.json.gz"));
    let samples: Vec<Vec<u8>> =
        data["samples"].as_array().unwrap().iter().map(|s| unhex(s.as_str().unwrap())).collect();
    let mut d = Diffs(vec![]);
    let cmaps = data["cmaps"].as_array().unwrap();
    let umaps = data["unicode_maps"].as_array().unwrap();
    assert_eq!(cmaps.len() + umaps.len() / 2, CMapDB::embedded_names().len());
    for c in cmaps {
        let name = c["name"].as_str().unwrap();
        let m = CMapDB::get_cmap(name).unwrap();
        d.check(|| format!("{name} repr"), json!(m.repr()), &c["repr"]);
        d.check(|| format!("{name} is_vertical"), json!(m.is_vertical()), &c["is_vertical"]);
        d.check(|| format!("{name} dump"), json!(cmap_dump_hash(&m)), &c["dump"]);
        for (s, want) in samples.iter().zip(c["decode"].as_array().unwrap()) {
            d.check(|| format!("{name} decode"), json!(m.decode(s).unwrap()), want);
        }
    }
    for u in umaps {
        let name = u["name"].as_str().unwrap();
        let v = u["vertical"].as_bool().unwrap();
        let m = CMapDB::get_unicode_map(name, v).unwrap();
        d.check(|| format!("{name}/{v} repr"), json!(m.repr()), &u["repr"]);
        d.check(|| format!("{name}/{v} is_vertical"), json!(m.is_vertical()), &u["is_vertical"]);
        d.check(|| format!("{name}/{v} dump"), json!(cmap_dump_hash(&m)), &u["dump"]);
    }
    let name_result = |r: PyResult<Rc<CMapBase>>| match r {
        Ok(_) => json!("ok"),
        Err(e) if is_cmap_not_found(&e) => json!("!CMapNotFound"),
        Err(e) => json!(exc(&e)),
    };
    for n in data["names"].as_array().unwrap() {
        let name = n[0].as_str().unwrap();
        d.check(|| format!("get_cmap({name:?})"), name_result(CMapDB::get_cmap(name)), &n[1]);
        d.check(|| format!("get_unicode_map({name:?})"), name_result(CMapDB::get_unicode_map(name, false)), &n[2]);
    }
    assert!(d.0.is_empty(), "{} differences:\n{}", d.0.len(), d.0.join("\n"));
}

fn cff_name(n: &CFFString) -> J {
    match n {
        CFFString::Str(s) => json!({"str": s}),
        CFFString::Bytes(b) => json!({"bytes": hex(b)}),
    }
}

fn cff_result(data: &[u8]) -> J {
    match CFFFont::new("x", BytesIO::from_vec(data.to_vec())) {
        Err(e) => json!(exc(&e)),
        Ok(f) => json!({
            "nglyphs": f.nglyphs,
            "code2gid": f.code2gid.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
            "gid2code": f.gid2code.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
            "name2gid": f.name2gid.iter().map(|(k, v)| json!([cff_name(k), v])).collect::<Vec<_>>(),
            "gid2name": f.gid2name.iter().map(|(k, v)| json!([k, cff_name(v)])).collect::<Vec<_>>(),
            "top_dict": f.top_dict.iter().map(|(k, v)| json!([k, v.iter().map(enc).collect::<Vec<_>>()])).collect::<Vec<_>>(),
        }),
    }
}

fn ttf_result(data: &[u8]) -> J {
    let mut f = TrueTypeFont::new("x", BytesIO::from_vec(data.to_vec()));
    let unicode_map = match f.create_unicode_map() {
        Ok(m) => json!(cmap_dump_hash(&m)),
        Err(e) if is_ttf_cmap_not_found(&e) => json!("!CMapNotFound"),
        Err(e) => json!(exc(&e)),
    };
    json!({
        "fonttype": hex(&f.fonttype),
        "tables": f.tables.iter().map(|(k, (o, l))| json!([hex(k), [o, l]])).collect::<Vec<_>>(),
        "unicode_map": unicode_map,
    })
}

fn font_file(spec: &Value, key: &str) -> Option<Vec<u8>> {
    let Value::Dict(spec) = spec else { return None };
    let desc = spec.borrow().get("FontDescriptor").cloned()?;
    let Value::Dict(desc) = desc else { return None };
    let ff = desc.borrow().get(key).cloned()?;
    match ff {
        Value::Stream(s) => Some(s.get_data().unwrap().to_vec()),
        _ => None,
    }
}

fn check_font(d: &mut Diffs, pdf: &str, idx: usize, rec: &J) {
    let class = rec["class"].as_str().unwrap();
    let spec = dec(&rec["spec"]);
    let Value::Dict(spec_dict) = &spec else { panic!("spec is not a dict") };
    let at = |what: &str| format!("{pdf} font #{idx} ({class}) {what}");
    let font = match class {
        "PDFType1Font" => PDFFont::new_type1(spec_dict),
        "PDFTrueTypeFont" => PDFFont::new_truetype(spec_dict),
        "PDFType3Font" => PDFFont::new_type3(spec_dict),
        "PDFCIDFont" => PDFFont::new_cid(spec_dict, false),
        c => panic!("unknown class {c}"),
    };
    let font = match font {
        Ok(f) => {
            if let Some(e) = rec.get("error") {
                d.check(|| at("constructor"), json!("ok"), e);
                return;
            }
            f
        }
        Err(e) => {
            d.check(|| at("constructor"), json!(exc(&e)), rec.get("error").unwrap_or(&json!("ok")));
            return;
        }
    };
    d.check(|| at("repr"), json!(font.repr()), &rec["repr"]);
    d.check(|| at("fontname"), enc(font.fontname()), &rec["fontname"]);
    d.check(|| at("is_vertical"), json!(font.is_vertical()), &rec["is_vertical"]);
    d.check(|| at("is_multibyte"), json!(font.is_multibyte()), &rec["is_multibyte"]);
    let fr = |r: PyResult<f64>| res(r, |x| json!(float_repr(x)));
    d.check(|| at("ascent"), fr(font.get_ascent()), &rec["ascent"]);
    d.check(|| at("descent"), fr(font.get_descent()), &rec["descent"]);
    d.check(|| at("width"), res(font.get_width_value(), |v| json!(v.repr())), &rec["width"]);
    d.check(|| at("height"), fr(font.get_height()), &rec["height"]);
    for dc in rec["decode"].as_array().unwrap() {
        let s = unhex(dc[0].as_str().unwrap());
        d.check(|| at(&format!("decode({})", dc[0])), res(font.decode(&s), |v| json!(v)), &dc[1]);
    }
    for sw in rec["string_width"].as_array().unwrap() {
        let s = unhex(sw[0].as_str().unwrap());
        d.check(
            || at(&format!("string_width({})", sw[0])),
            res(font.string_width_value(&s), |v| json!(v.repr())),
            &sw[1],
        );
    }
    for c in rec["cids"].as_array().unwrap() {
        let cid = c[0].as_i64().unwrap();
        d.check(|| at(&format!("char_width({cid})")), res(font.char_width_value(cid), |v| json!(v.repr())), &c[1]);
        d.check(|| at(&format!("char_disp({cid})")), res(font.char_disp(cid), |v| enc(&v)), &c[2]);
        d.check(|| at(&format!("to_unichr({cid})")), res(font.to_unichr(cid), |s| pystr_cps(&s)), &c[3]);
    }
    if let Some(want) = rec.get("cff") {
        d.check(|| at("cff"), cff_result(&font_file(&spec, "FontFile3").unwrap()), want);
    }
    if let Some(want) = rec.get("ttf") {
        d.check(|| at("ttf"), ttf_result(&font_file(&spec, "FontFile2").unwrap()), want);
    }
}

#[test]
fn sample_fonts_match_python() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(data_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap() != "cmaps.json.gz" && p.file_name().unwrap() != "fuzz.json.gz")
        .collect();
    files.sort();
    let mut d = Diffs(vec![]);
    let mut n = 0;
    for f in &files {
        let data = load(f);
        let pdf = data["pdf"].as_str().unwrap();
        for (i, rec) in data["fonts"].as_array().unwrap().iter().enumerate() {
            check_font(&mut d, pdf, i, rec);
            n += 1;
        }
    }
    assert!(n > 200, "only {n} fonts");
    assert!(d.0.is_empty(), "{} differences:\n{}", d.0.len(), d.0.join("\n"));
}

/// Random inputs (from `fuzz_main` in the generator) that exercise the error paths: mutated font
/// specs, ToUnicode CMaps, Type1 headers, glyph names, TrueType cmap tables and CFF fonts.
#[test]
fn fuzz_cases_match_python() {
    use pdfminer::cmapdb::CMapParser;
    use pdfminer::encodingdb::name2unicode;
    use pdfminer::pdffont::Type1FontHeaderParser;

    let data = load(&data_dir().join("fuzz.json.gz"));
    let mut d = Diffs(vec![]);
    for (i, rec) in data["specs"].as_array().unwrap().iter().enumerate() {
        check_font(&mut d, "fuzz", i, rec);
    }
    for (i, c) in data["tounicode"].as_array().unwrap().iter().enumerate() {
        let input = unhex(c[0].as_str().unwrap());
        let mut m = CMapBase::new_file_unicode_map();
        let r = CMapParser::new(&mut m, BytesIO::from_vec(input)).run();
        let got = match r {
            Ok(()) => json!(cmap_dump_hash(&m)),
            Err(e) => json!(exc(&e)),
        };
        d.check(|| format!("tounicode #{i} {}", c[0]), got, &c[1]);
    }
    for (i, c) in data["type1"].as_array().unwrap().iter().enumerate() {
        let input = unhex(c[0].as_str().unwrap());
        let got = res(Type1FontHeaderParser::new(BytesIO::from_vec(input)).get_encoding(), |e| {
            let mut items: Vec<_> = e.into_iter().collect();
            items.sort_by_key(|(k, _)| *k);
            J::Array(items.into_iter().map(|(k, v)| json!([k, pystr_cps(&v)])).collect())
        });
        d.check(|| format!("type1 #{i} {}", c[0]), got, &c[1]);
    }
    for c in data["names"].as_array().unwrap() {
        let name = cps_string(&c[0]);
        d.check(|| format!("name2unicode({name:?})"), res(name2unicode(&name), |s| pystr_cps(&s)), &c[1]);
    }
    for (i, c) in data["ttf"].as_array().unwrap().iter().enumerate() {
        d.check(|| format!("ttf #{i} {}", c[0]), ttf_result(&unhex(c[0].as_str().unwrap())), &c[1]);
    }
    for (i, c) in data["cff"].as_array().unwrap().iter().enumerate() {
        d.check(|| format!("cff #{i} {}", c[0]), cff_result(&unhex(c[0].as_str().unwrap())), &c[1]);
    }
    assert!(d.0.is_empty(), "{} differences:\n{}", d.0.len(), d.0.join("\n"));
}
