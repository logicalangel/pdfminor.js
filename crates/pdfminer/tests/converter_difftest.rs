//! The converters and the layout dump against pdfminer.six, on hand-built pages.
//!
//! `tools/gen_testdata/converter.py` replayed each scenario (the calls the interpreter makes on a
//! `PDFLayoutAnalyzer`: begin_page, characters, paint_path, figures, images) on every converter
//! configuration and recorded the bytes written (and the exception, with the partial output).
//! This test replays the same calls on the port's converters and compares.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::rc::Rc;

use pdfminer::converter::{
    HOCRConverter, HTMLConverter, HTMLConverterParams, LayoutAnalyzerDevice, Num, PDFPageAggregator, TextConverter,
    XMLConverter,
};
use pdfminer::layout::{LAParams, LTChar_from_font_values};
use pdfminer::pdfcolor::predefined;
use pdfminer::pdfinterp::{Color, PDFGraphicState, PDFGraphicStateRef, PDFResourceManager, StandardColor};
use pdfminer::pdftypes::{PDFObjRef, PDFStream};
use pdfminer::psparser::{KWD, LIT};
use pdfminer::pyrt::PyStr;
use pdfminer::pyrt::error::{PyErr, PyResult};
use pdfminer::pyrt::textio::{AnyIO, BytesOut, Codec, Errors, StringIO, TextFile};
use pdfminer::pyrt::value::Value;
use pdfminer::utils::{Matrix, PathSegment};
use serde_json::Value as Json;

fn load() -> Json {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/converter/converter.json.gz");
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap()).read_to_string(&mut s).unwrap();
    serde_json::from_str(&s).unwrap()
}

fn unhex(h: &str) -> Vec<u8> {
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect()
}

fn val(v: &Json) -> Value {
    match v {
        Json::Null => Value::None,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => Value::Int(n.as_i64().unwrap()),
        Json::Object(o) => {
            let (k, x) = o.iter().next().unwrap();
            match k.as_str() {
                "f" => Value::Float(x.as_str().unwrap().parse().unwrap()),
                "s" => Value::str(x.as_str().unwrap()),
                "y" => Value::bytes(&unhex(x.as_str().unwrap())),
                "t" => Value::tuple(x.as_array().unwrap().iter().map(val).collect()),
                "l" => Value::list(x.as_array().unwrap().iter().map(val).collect()),
                "lit" => Value::Literal(LIT(x.as_str().unwrap())),
                "ref" => Value::Ref(PDFObjRef::new(None, x.as_i64().unwrap()).unwrap()),
                "kwd" => Value::Keyword(KWD(x.as_str().unwrap().as_bytes())),
                "d" => {
                    let d = Value::new_dict();
                    for kv in x.as_array().unwrap() {
                        d.borrow_mut().insert(kv[0].as_str().unwrap().into(), val(&kv[1]));
                    }
                    Value::Dict(d)
                }
                _ => panic!("unknown value {v}"),
            }
        }
        _ => panic!("unknown value {v}"),
    }
}

fn num(v: &Json) -> f64 {
    val(v).as_f64().unwrap()
}

fn matrix(v: &Json) -> Matrix {
    let a: Vec<f64> = v.as_array().unwrap().iter().map(num).collect();
    (a[0], a[1], a[2], a[3], a[4], a[5])
}

fn floats(v: &Json) -> Vec<f64> {
    v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect()
}

fn standard(c: &Json) -> StandardColor {
    let o = c.as_object().unwrap();
    if let Some(i) = o.get("int") {
        StandardColor::IntGray(i.as_i64().unwrap())
    } else if let Some(g) = o.get("gray") {
        StandardColor::Gray(g.as_f64().unwrap())
    } else if let Some(a) = o.get("rgb") {
        let a = floats(a);
        StandardColor::Rgb(a[0], a[1], a[2])
    } else {
        let a = floats(&o["cmyk"]);
        StandardColor::Cmyk(a[0], a[1], a[2], a[3])
    }
}

fn color(c: &Json) -> Color {
    let o = c.as_object().unwrap();
    if let Some(base) = o.get("base") {
        Color::UncoloredPattern(standard(base), o["pattern"].as_str().unwrap().to_string())
    } else if let Some(p) = o.get("pattern") {
        Color::Pattern(p.as_str().unwrap().to_string())
    } else {
        Color::Standard(standard(c))
    }
}

fn graphicstate(spec: &Json) -> PDFGraphicStateRef {
    let mut gs = PDFGraphicState::new();
    for (k, v) in spec.as_object().unwrap() {
        match k.as_str() {
            "ncolor" => gs.ncolor = color(v),
            "scolor" => gs.scolor = color(v),
            "ncs" => gs.ncs = predefined(v.as_str().unwrap()).unwrap(),
            "scs" => gs.scs = predefined(v.as_str().unwrap()).unwrap(),
            "linewidth" => gs.linewidth = val(v),
            "dash" => {
                let Value::Tuple(t) = val(v) else { panic!("dash") };
                gs.dash = Some((t[0].clone(), t[1].clone()));
            }
            _ => panic!("gs key {k}"),
        }
    }
    Rc::new(RefCell::new(gs))
}

fn text(v: &Json) -> PyStr {
    match v {
        Json::String(s) => PyStr::from(s.as_str()),
        _ => PyStr::from_code_points(v["cps"].as_array().unwrap().iter().map(|c| c.as_u64().unwrap() as u32)),
    }
}

/// Replays the operations, then `end_page` and `close`.
fn replay<D: LayoutAnalyzerDevice>(dev: &mut D, ops: &[Json]) -> PyResult<()> {
    let mut gstates: HashMap<i64, PDFGraphicStateRef> = HashMap::new();
    for op in ops {
        match op["op"].as_str().unwrap() {
            "page" => {
                let ctm = matrix(&op["ctm"]);
                dev.analyzer_mut().ctm = Some(ctm);
                let mb: Vec<f64> = op["mediabox"].as_array().unwrap().iter().map(num).collect();
                dev.analyzer_mut().begin_page_mediabox((mb[0], mb[1], mb[2], mb[3]), ctm)?;
            }
            "ctm" => dev.analyzer_mut().ctm = Some(matrix(&op["ctm"])),
            "char" => {
                let gid = op["gs_id"].as_i64().unwrap();
                let gs = gstates.entry(gid).or_insert_with(|| graphicstate(&op["gs"])).clone();
                let item = LTChar_from_font_values(
                    matrix(&op["matrix"]),
                    val(&op["fontname"]),
                    op["vertical"].as_bool().unwrap(),
                    num(&op["descent"]),
                    num(&op["fontsize"]),
                    num(&op["scaling"]),
                    num(&op["rise"]),
                    text(&op["text"]),
                    num(&op["textwidth"]),
                    &val(&op["textdisp"]),
                    predefined(op["ncs"].as_str().unwrap()).unwrap(),
                    gs,
                )?;
                dev.analyzer_mut().cur_item.as_ref().unwrap().add(item);
            }
            "path" => {
                let path: Vec<PathSegment> = op["path"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| {
                        let s = s.as_array().unwrap();
                        PathSegment {
                            op: s[0].as_str().unwrap().chars().next().unwrap(),
                            args: s[1..].iter().map(num).collect(),
                        }
                    })
                    .collect();
                let gs = graphicstate(&op["gs"]);
                LayoutAnalyzerDevice::paint_path(
                    dev,
                    &gs,
                    op["stroke"].as_bool().unwrap(),
                    op["fill"].as_bool().unwrap(),
                    op["evenodd"].as_bool().unwrap(),
                    &path,
                )?;
            }
            "begin_figure" => {
                let bbox: Vec<Value> = op["bbox"].as_array().unwrap().iter().map(val).collect();
                let m: Vec<Value> = op["matrix"].as_array().unwrap().iter().map(val).collect();
                dev.analyzer_mut().begin_figure(op["name"].as_str().unwrap(), &bbox, &m)?;
            }
            "end_figure" => dev.analyzer_mut().end_figure(op["name"].as_str().unwrap())?,
            "image" => {
                let Value::Dict(attrs) = val(&op["attrs"]) else { panic!("attrs") };
                let stream = Rc::new(PDFStream::new(attrs, Rc::from(&b""[..]), None));
                if let Some(objid) = op.get("objid") {
                    stream.set_objid(objid.as_i64().unwrap(), 0);
                }
                LayoutAnalyzerDevice::render_image(dev, op["name"].as_str().unwrap(), &stream)?;
            }
            other => panic!("op {other}"),
        }
    }
    let page = dev.analyzer_mut().end_page()?;
    dev.receive_layout(page)?;
    dev.close()
}

fn laparams(spec: &Json) -> Option<LAParams> {
    if spec.is_null() {
        return None;
    }
    let mut la = LAParams::default();
    for (k, v) in spec.as_object().unwrap() {
        match k.as_str() {
            "boxes_flow" => la.boxes_flow = v.as_f64(),
            "detect_vertical" => la.detect_vertical = v.as_bool().unwrap(),
            "all_texts" => la.all_texts = v.as_bool().unwrap(),
            "char_margin" => la.char_margin = v.as_f64().unwrap(),
            "word_margin" => la.word_margin = v.as_f64().unwrap(),
            "line_margin" => la.line_margin = v.as_f64().unwrap(),
            "line_overlap" => la.line_overlap = v.as_f64().unwrap(),
            _ => panic!("laparams {k}"),
        }
    }
    Some(la)
}

type Outcome = (Vec<u8>, Option<(String, String)>);

fn outcome<D: LayoutAnalyzerDevice>(dev: PyResult<D>, ops: &[Json], get: &dyn Fn() -> Vec<u8>) -> Outcome {
    let err = |e: PyErr| Some((e.kind.name().to_string(), e.msg));
    let e = match dev {
        Ok(mut d) => replay(&mut d, ops).err(),
        Err(e) => Some(e),
    };
    (get(), e.and_then(err))
}

/// Runs a scenario on one configuration: the output bytes, and the exception if one was raised.
fn run(config: &str, ops: &[Json], lp: Option<LAParams>) -> Outcome {
    let rm = PDFResourceManager::new(true);
    let string_io = Rc::new(RefCell::new(StringIO::new()));
    let bytes_out = Rc::new(RefCell::new(BytesOut::new()));
    let get_str = || string_io.borrow().getvalue().as_bytes().to_vec();
    let get_bytes = || bytes_out.borrow().getvalue().to_vec();
    let html = |p: HTMLConverterParams, out: AnyIO| HTMLConverter::new(rm.clone(), out, p);
    match config {
        "text_str" | "text_str_pageno" => {
            let d =
                TextConverter::new(rm.clone(), string_io.clone(), "utf-8", 1, lp, config == "text_str_pageno", None);
            outcome(Ok(d), ops, &get_str)
        }
        "text_bytes" => {
            let d = TextConverter::new(rm.clone(), bytes_out.clone(), "utf-8", 1, lp, true, None);
            outcome(Ok(d), ops, &get_bytes)
        }
        "text_tio_strict" | "text_tio_escape" => {
            let errors = if config == "text_tio_strict" { Errors::Strict } else { Errors::SurrogateEscape };
            let tio = Rc::new(RefCell::new(TextFile::new(Vec::<u8>::new(), Codec::Utf8, errors)));
            let d = TextConverter::new(rm.clone(), tio.clone(), "utf-8", 1, lp, false, None);
            outcome(Ok(d), ops, &|| tio.borrow().inner.clone())
        }
        "html" => outcome(
            html(HTMLConverterParams { laparams: lp, ..Default::default() }, bytes_out.clone()),
            ops,
            &get_bytes,
        ),
        "html_exact_debug" => outcome(
            html(
                HTMLConverterParams {
                    laparams: lp,
                    scale: Num::Float(1.5),
                    layoutmode: "exact".into(),
                    debug: 1,
                    ..Default::default()
                },
                bytes_out.clone(),
            ),
            ops,
            &get_bytes,
        ),
        "html_loose" => outcome(
            html(
                HTMLConverterParams {
                    laparams: lp,
                    scale: Num::Int(2),
                    fontscale: Num::Float(0.5),
                    layoutmode: "loose".into(),
                    showpageno: false,
                    pagemargin: 7,
                    ..Default::default()
                },
                bytes_out.clone(),
            ),
            ops,
            &get_bytes,
        ),
        "html_latin1" => outcome(
            html(
                HTMLConverterParams { laparams: lp, codec: "latin-1".into(), ..Default::default() },
                bytes_out.clone(),
            ),
            ops,
            &get_bytes,
        ),
        "html_str" => outcome(
            html(
                HTMLConverterParams {
                    laparams: lp,
                    codec: String::new(),
                    layoutmode: "exact".into(),
                    ..Default::default()
                },
                string_io.clone(),
            ),
            ops,
            &get_str,
        ),
        "xml" | "xml_strip" | "xml_latin1" => {
            let codec = if config == "xml_latin1" { "latin-1" } else { "utf-8" };
            let d = XMLConverter::new(rm.clone(), bytes_out.clone(), codec, 1, lp, None, config == "xml_strip");
            outcome(d, ops, &get_bytes)
        }
        "xml_str" => outcome(XMLConverter::new(rm.clone(), string_io.clone(), "", 1, lp, None, false), ops, &get_str),
        "hocr" | "hocr_strip" => {
            let (codec, strip) = if config == "hocr" { ("utf8", false) } else { ("utf-8", true) };
            outcome(HOCRConverter::new(rm.clone(), bytes_out.clone(), codec, 1, lp, strip), ops, &get_bytes)
        }
        "dump" => {
            let mut d = PDFPageAggregator::new(rm.clone(), 1, lp);
            match replay(&mut d, ops) {
                Ok(()) => match pdfminer::dump::dump_page(&d.get_result().unwrap()) {
                    Ok(s) => (s.into_bytes(), None),
                    Err(e) => (vec![], Some((e.kind.name().to_string(), e.msg))),
                },
                Err(e) => (vec![], Some((e.kind.name().to_string(), e.msg))),
            }
        }
        other => panic!("config {other}"),
    }
}

#[test]
fn converters_match_pdfminer() {
    let data = load();
    let mut failures = Vec::new();
    let mut n = 0;
    for case in data["cases"].as_array().unwrap() {
        let sc = &case["scenario"];
        let name = sc["name"].as_str().unwrap();
        let ops = sc["ops"].as_array().unwrap();
        for (config, expected) in case["results"].as_object().unwrap() {
            n += 1;
            let (got, err) = run(config, ops, laparams(&sc["laparams"]));
            let exp_err = expected
                .get("err")
                .map(|e| (e.as_str().unwrap().to_string(), expected["msg"].as_str().unwrap().to_string()));
            if exp_err != err {
                failures.push(format!("{name} / {config}: exception python {exp_err:?}, rust {err:?}"));
                continue;
            }
            if let Some(exp_out) = expected.get("out") {
                let exp = unhex(exp_out.as_str().unwrap());
                if exp != got {
                    let i = exp.iter().zip(&got).position(|(a, b)| a != b).unwrap_or(exp.len().min(got.len()));
                    let lo = i.saturating_sub(100);
                    failures.push(format!(
                        "{name} / {config}: output differs at byte {i}\n  python: {}\n  rust:   {}",
                        String::from_utf8_lossy(&exp[lo..(i + 100).min(exp.len())]),
                        String::from_utf8_lossy(&got[lo..(i + 100).min(got.len())])
                    ));
                }
            }
        }
    }
    for f in &failures {
        eprintln!("{f}");
    }
    assert!(failures.is_empty(), "{} of {n} converter runs differ from pdfminer.six", failures.len());
}
