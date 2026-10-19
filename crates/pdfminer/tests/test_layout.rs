//! Port of `tests/test_layout.py`, and the differential test of the layout analysis against
//! pdfminer.six.
//!
//! The upstream tests that read sample PDFs (`test_pdf_with_empty_characters_*`) run on the
//! pages recorded from those PDFs, since the port has no PDF pipeline of its own here.
//!
//! `tools/gen_testdata/layout.py` recorded, for pages of the sample PDFs and synthetic pages,
//! what pdfminer.six's analyzer received (the characters with the arguments they were built
//! from, curves, images, figures) and the analysed tree under several LAParams. This test
//! rebuilds each page with the port's constructors, checks the constructed objects, runs
//! `analyze` and compares the tree (types, bboxes as float repr, text, indices, groups).

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;

use pdfminer::layout::{
    LAParams, LTChar_from_attrs, LTChar_from_font_values, LTCurve, LTFigure, LTImage, LTItem, LTLayoutContainer,
    LTLine, LTPage, LTRect, LTTextLineHorizontal, LTTextLineVertical,
};
use pdfminer::pdfcolor::predefined;
use pdfminer::pdfinterp::PDFGraphicState;
use pdfminer::pdftypes::PDFStream;
use pdfminer::psparser::LIT;
use pdfminer::pyrt::PyStr;
use pdfminer::pyrt::fmt::float_repr;
use pdfminer::pyrt::value::Value;
use pdfminer::utils::{Plane, Rect};
use serde_json::{Value as Json, json};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/layout")
}

fn load(path: &PathBuf) -> Json {
    let raw = std::fs::read(path).unwrap();
    let mut s = String::new();
    flate2::read::GzDecoder::new(&raw[..]).read_to_string(&mut s).unwrap();
    serde_json::from_str(&s).unwrap()
}

fn f(v: &Json) -> f64 {
    let s = v.as_str().unwrap_or_else(|| panic!("float repr expected: {v}"));
    s.parse::<f64>().unwrap_or_else(|_| panic!("bad float {s}"))
}

fn rect(v: &Json) -> (f64, f64, f64, f64) {
    let a = v.as_array().unwrap();
    (f(&a[0]), f(&a[1]), f(&a[2]), f(&a[3]))
}

fn matrix(v: &Json) -> (f64, f64, f64, f64, f64, f64) {
    let a = v.as_array().unwrap();
    (f(&a[0]), f(&a[1]), f(&a[2]), f(&a[3]), f(&a[4]), f(&a[5]))
}

fn text(v: &Json) -> PyStr {
    match v {
        Json::String(s) => PyStr::from(s.as_str()),
        Json::Object(o) => {
            PyStr::from_code_points(o["cps"].as_array().unwrap().iter().map(|c| c.as_u64().unwrap() as u32))
        }
        _ => panic!("text expected: {v}"),
    }
}

fn text_json(s: &PyStr) -> Json {
    if s.code_points().any(|c| (0xD800..=0xDFFF).contains(&c)) {
        json!({"cps": s.code_points().collect::<Vec<u32>>()})
    } else {
        Json::String(s.as_str().unwrap().to_string())
    }
}

/// A Python object recorded by the generator's `enc`.
fn value(v: &Json) -> Value {
    match v {
        Json::Null => Value::None,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => Value::Int(n.as_i64().unwrap()),
        Json::Object(o) => {
            let (k, x) = o.iter().next().unwrap();
            match k.as_str() {
                "f" => Value::Float(f(x)),
                "s" => match x {
                    Json::String(s) => Value::str(s),
                    _ => panic!("surrogates in a str value"),
                },
                "y" => {
                    let h = x.as_str().unwrap();
                    Value::bytes(
                        &(0..h.len())
                            .step_by(2)
                            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
                            .collect::<Vec<_>>(),
                    )
                }
                "t" => Value::tuple(x.as_array().unwrap().iter().map(value).collect()),
                "l" => Value::list(x.as_array().unwrap().iter().map(value).collect()),
                "lit" => Value::Literal(LIT(x.as_str().unwrap())),
                _ => panic!("unknown value {v}"),
            }
        }
        _ => panic!("unknown value {v}"),
    }
}

fn reprs(r: (f64, f64, f64, f64)) -> Json {
    json!([float_repr(r.0), float_repr(r.1), float_repr(r.2), float_repr(r.3)])
}

struct Builder {
    index: HashMap<LTItem, usize>,
    problems: Vec<String>,
}

impl Builder {
    fn check(&mut self, what: &str, got: Json, want: &Json) {
        if &got != want {
            self.problems.push(format!("{what}: got {got}, want {want}"));
        }
    }

    fn obj(&mut self, d: &Json) -> LTItem {
        let i = self.index.len();
        let kind = d["k"].as_str().unwrap();
        let bbox = rect(&d["bbox"]);
        let gs = PDFGraphicState::new().copy();
        let ncs = predefined("DeviceGray").unwrap();
        let item = match kind {
            "LTChar" if d.get("fontsize").is_some() => {
                let descent = if d["descent"].is_null() { 0.0 } else { f(&d["descent"]) };
                let c = LTChar_from_font_values(
                    matrix(&d["matrix"]),
                    value(&d["fontname"]),
                    d["vertical"].as_bool().unwrap(),
                    descent,
                    f(&d["fontsize"]),
                    f(&d["scaling"]),
                    f(&d["rise"]),
                    text(&d["text"]),
                    f(&d["textwidth"]),
                    &value(&d["textdisp"]),
                    ncs,
                    gs,
                )
                .unwrap_or_else(|e| panic!("LTChar raised {e:?} for {d}"));
                let a = c.as_char().unwrap();
                self.check(&format!("char {i} bbox"), reprs(c.bbox()), &d["bbox"]);
                self.check(&format!("char {i} adv"), json!(float_repr(a.adv)), &d["adv"]);
                self.check(&format!("char {i} size"), json!(float_repr(a.size)), &d["size"]);
                self.check(&format!("char {i} upright"), json!(a.upright), &d["upright"]);
                c
            }
            "LTChar" => LTChar_from_attrs(
                text(&d["text"]),
                matrix(&d["matrix"]),
                Value::None,
                f(&d["adv"]),
                d["upright"].as_bool().unwrap(),
                f(&d["size"]),
                bbox,
                ncs,
                gs,
            ),
            "LTCurve" | "LTLine" | "LTRect" => {
                let pts: Vec<(f64, f64)> = d["pts"].as_array().unwrap().iter().map(|p| (f(&p[0]), f(&p[1]))).collect();
                let lw = value(&d["linewidth"]);
                let c = match kind {
                    "LTCurve" => LTCurve(lw, pts, false, false, false, None, None, None, None),
                    "LTLine" => LTLine(lw, pts[0], pts[1], false, false, false, None, None, None, None),
                    _ => LTRect(
                        lw,
                        (pts[0].0, pts[0].1, pts[2].0, pts[2].1),
                        false,
                        false,
                        false,
                        None,
                        None,
                        None,
                        None,
                    ),
                };
                self.check(&format!("{kind} {i} bbox"), reprs(c.bbox()), &d["bbox"]);
                c
            }
            "LTImage" => {
                let stream = Rc::new(PDFStream::new(Value::new_dict(), Rc::from(&b""[..]), None));
                LTImage(d["name"].as_str().unwrap(), stream, bbox)
            }
            "LTFigure" => {
                let name = d["name"].as_str().unwrap();
                let fig = if d["fbbox"].is_null() {
                    let fig = LTFigure(name, (0.0, 0.0, 0.0, 0.0), matrix(&d["matrix"]));
                    fig.set_bbox(bbox);
                    fig
                } else {
                    LTFigure(name, rect(&d["fbbox"]), matrix(&d["matrix"]))
                };
                self.check(&format!("figure {i} bbox"), reprs(fig.bbox()), &d["bbox"]);
                fig
            }
            _ => panic!("unknown kind {kind}"),
        };
        self.index.insert(item.clone(), i);
        if kind == "LTFigure" {
            for c in d["objs"].as_array().unwrap() {
                let child = self.obj(c);
                item.add(child);
            }
        }
        item
    }

    fn page(&mut self, rec: &Json) -> LTItem {
        let page = LTPage(rec["pageid"].as_i64().unwrap(), rect(&rec["bbox"]), value(&rec["rotate"]));
        for d in rec["objs"].as_array().unwrap() {
            let o = self.obj(d);
            page.add(o);
        }
        page
    }

    fn node(&self, o: &LTItem) -> Json {
        if let Some(&i) = self.index.get(o) {
            if o.is_ltfigure() {
                return json!({"f": i, "c": o.objs().iter().map(|x| self.node(x)).collect::<Vec<_>>(), "g": self.groups(o)});
            }
            return json!(i);
        }
        if o.is_ltanno() {
            return json!({"a": text_json(&o.get_text())});
        }
        let mut d = serde_json::Map::new();
        d.insert("t".into(), json!(o.class_name()));
        d.insert("b".into(), reprs(o.bbox()));
        d.insert("c".into(), Json::Array(o.objs().iter().map(|x| self.node(x)).collect()));
        if o.is_lttextbox() {
            d.insert("x".into(), json!(o.index()));
        }
        Json::Object(d)
    }

    /// The group trees, flattened like the generator does (groups in post-order).
    fn groups(&self, o: &LTItem) -> Json {
        let Some(groups) = o.groups() else {
            return Json::Null;
        };
        let mut nodes: Vec<Json> = Vec::new();
        let mut roots = Vec::new();
        for g in &groups {
            let mut stack = vec![(g.clone(), false)];
            let mut refs: Vec<Json> = Vec::new();
            while let Some((x, expanded)) = stack.pop() {
                if !x.is_lttextgroup() {
                    refs.push(json!({"box": x.index()}));
                } else if expanded {
                    let n = x.len();
                    let children = refs.split_off(refs.len() - n);
                    nodes.push(json!({"t": x.class_name(), "b": reprs(x.bbox()), "c": children}));
                    refs.push(json!({"grp": nodes.len() - 1}));
                } else {
                    stack.push((x.clone(), true));
                    for c in x.objs().into_iter().rev() {
                        stack.push((c, false));
                    }
                }
            }
            roots.push(refs.pop().unwrap());
        }
        json!({"nodes": nodes, "roots": roots})
    }
}

fn laparams(s: &Json) -> LAParams {
    LAParams::new(
        f(&s["line_overlap"]),
        f(&s["char_margin"]),
        f(&s["line_margin"]),
        f(&s["word_margin"]),
        if s["boxes_flow"].is_null() { None } else { Some(f(&s["boxes_flow"])) },
        s["detect_vertical"].as_bool().unwrap(),
        s["all_texts"].as_bool().unwrap(),
    )
    .unwrap()
}

/// The first place where two JSON trees differ.
fn first_diff(path: &str, got: &Json, want: &Json) -> Option<String> {
    match (got, want) {
        (Json::Array(a), Json::Array(b)) => {
            for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
                if let Some(d) = first_diff(&format!("{path}[{i}]"), x, y) {
                    return Some(d);
                }
            }
            if a.len() != b.len() {
                return Some(format!("{path}: length {} != {}", a.len(), b.len()));
            }
            None
        }
        (Json::Object(a), Json::Object(b)) => {
            for (k, y) in b {
                match a.get(k) {
                    Some(x) => {
                        if let Some(d) = first_diff(&format!("{path}.{k}"), x, y) {
                            return Some(d);
                        }
                    }
                    None => {
                        let (g, w) = (got.to_string(), want.to_string());
                        return Some(format!(
                            "{path}.{k} missing: got {}, want {}",
                            &g[..g.len().min(300)],
                            &w[..w.len().min(300)]
                        ));
                    }
                }
            }
            if a.len() != b.len() {
                return Some(format!(
                    "{path}: keys {:?} != {:?}",
                    a.keys().collect::<Vec<_>>(),
                    b.keys().collect::<Vec<_>>()
                ));
            }
            None
        }
        _ if got == want => None,
        _ => Some(format!("{path}: got {got}, want {want}")),
    }
}

/// Analyses one recorded page under one setting; the result in the generator's format.
fn analyse(rec: &Json, lp: &LAParams) -> (Builder, LTItem, Json) {
    let mut b = Builder { index: HashMap::new(), problems: Vec::new() };
    let page = b.page(rec);
    let got = match page.analyze(lp) {
        Ok(()) => {
            json!({"c": page.objs().iter().map(|x| b.node(x)).collect::<Vec<_>>(), "g": b.groups(&page)})
        }
        Err(e) => json!({"error": e.kind.name(), "msg": e.msg}),
    };
    (b, page, got)
}

fn run_file(path: &PathBuf, failures: &mut Vec<String>) -> usize {
    let data = load(path);
    let settings = &data["settings"];
    let mut n = 0;
    for (pi, page) in data["pages"].as_array().unwrap().iter().enumerate() {
        let label = format!(
            "{} page {pi}{}",
            data["file"].as_str().unwrap(),
            page.get("name").map_or(String::new(), |n| format!(" ({n})"))
        );
        for (name, want) in page["results"].as_object().unwrap() {
            let lp = laparams(&settings[name]);
            let (b, _page, got) = analyse(&page["input"], &lp);
            for p in &b.problems {
                failures.push(format!("{label} [{name}] construction: {p}"));
            }
            if let Some(d) = first_diff("", &got, want) {
                failures.push(format!("{label} [{name}]: {d}"));
            }
            n += 1;
        }
    }
    n
}

#[test]
fn layout_analysis_matches_pdfminer() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(data_dir()).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    assert!(files.len() > 10, "no test data: run tools/gen_testdata/layout.py");
    let mut failures = Vec::new();
    let mut n = 0;
    for path in &files {
        n += run_file(path, &mut failures);
    }
    if !failures.is_empty() {
        for f in failures.iter().take(40) {
            eprintln!("{f}");
        }
        panic!("{} of {} analyses differ from pdfminer.six", failures.len(), n);
    }
    eprintln!("{n} analyses match");
}

fn first_page(file: &str) -> Json {
    let data = load(&data_dir().join(file));
    data["pages"][0]["input"].clone()
}

/// Regression test for issue #449 (upstream `test_pdf_with_empty_characters_horizontal`):
/// the page aggregator should separate the 3 horizontal lines in the sample PDF. The used PDF
/// sample has multiple explicit space characters in between lines with text.
#[test]
fn test_pdf_with_empty_characters_horizontal() {
    let rec = first_page("contrib__issue-449-horizontal.json.gz");
    let (_b, page, _) = analyse(&rec, &LAParams::default());
    let textboxes: Vec<LTItem> = page.objs().into_iter().filter(|o| o.is_lttextboxhorizontal()).collect();
    assert_eq!(textboxes.len(), 3);
}

/// Regression test for issue #449 (upstream `test_pdf_with_empty_characters_vertical`).
#[test]
fn test_pdf_with_empty_characters_vertical() {
    let rec = first_page("contrib__issue-449-vertical.json.gz");
    let laparams = LAParams { detect_vertical: true, ..LAParams::default() };
    let (_b, page, _) = analyse(&rec, &laparams);
    let textboxes: Vec<LTItem> = page.objs().into_iter().filter(|o| o.is_lttextboxvertical()).collect();
    assert_eq!(textboxes.len(), 3);
}

// --- Ported from tests/test_layout.py ---------------------------------------------------------

fn rect_line(class: fn(f64) -> LTItem, bbox: Rect) -> LTItem {
    let line = class(LAParams::default().word_margin);
    line.set_bbox(bbox);
    line
}

// TestGroupTextLines
#[test]
fn test_parent_with_wrong_bbox_returns_non_empty_neighbour_list() {
    // LTLayoutContainer.group_textlines() should return all the lines in a separate
    // LTTextBoxes if they do not overlap. Even when the bounding box of the parent
    // container does not contain all the lines.
    let laparams = LAParams::default();
    let layout = LTLayoutContainer((0.0, 0.0, 50.0, 50.0));
    let line1 = rect_line(LTTextLineHorizontal, (0.0, 0.0, 50.0, 5.0));
    let line2 = rect_line(LTTextLineHorizontal, (0.0, 50.0, 50.0, 55.0));
    let lines = vec![line1, line2];

    let textboxes = layout.group_textlines(&laparams, &lines).unwrap();

    assert_eq!(textboxes.len(), 2);
}

fn assert_count_equal(a: &[LTItem], b: &[LTItem]) {
    assert_eq!(a.len(), b.len());
    for x in b {
        assert!(a.contains(x), "{x:?} missing");
    }
}

// TestFindNeigbors
#[test]
fn test_find_neighbors_horizontal() {
    let laparams = LAParams::default();
    let mut plane = Plane::new((0.0, 0.0, 50.0, 50.0), 50);
    let h = LTTextLineHorizontal;

    let line = rect_line(h, (10.0, 4.0, 20.0, 6.0));
    plane.add(line.clone()).unwrap();
    let left_aligned_above = rect_line(h, (10.0, 6.0, 15.0, 8.0));
    plane.add(left_aligned_above.clone()).unwrap();
    let right_aligned_below = rect_line(h, (15.0, 2.0, 20.0, 4.0));
    plane.add(right_aligned_below.clone()).unwrap();
    let centrally_aligned_overlapping = rect_line(h, (13.0, 5.0, 17.0, 7.0));
    plane.add(centrally_aligned_overlapping.clone()).unwrap();
    let not_aligned = rect_line(h, (0.0, 6.0, 5.0, 8.0));
    plane.add(not_aligned).unwrap();
    let wrong_height = rect_line(h, (10.0, 6.0, 15.0, 10.0));
    plane.add(wrong_height).unwrap();

    let neighbors = line.find_neighbors(&plane, laparams.line_margin).unwrap();
    assert_count_equal(&neighbors, &[line, left_aligned_above, right_aligned_below, centrally_aligned_overlapping]);
}

#[test]
fn test_find_neighbors_vertical() {
    let laparams = LAParams::default();
    let mut plane = Plane::new((0.0, 0.0, 50.0, 50.0), 50);
    let v = LTTextLineVertical;

    let line = rect_line(v, (4.0, 10.0, 6.0, 20.0));
    plane.add(line.clone()).unwrap();
    let bottom_aligned_right = rect_line(v, (6.0, 10.0, 8.0, 15.0));
    plane.add(bottom_aligned_right.clone()).unwrap();
    let top_aligned_left = rect_line(v, (2.0, 15.0, 4.0, 20.0));
    plane.add(top_aligned_left.clone()).unwrap();
    let centrally_aligned_overlapping = rect_line(v, (5.0, 13.0, 7.0, 17.0));
    plane.add(centrally_aligned_overlapping.clone()).unwrap();
    let not_aligned = rect_line(v, (6.0, 0.0, 8.0, 5.0));
    plane.add(not_aligned).unwrap();
    let wrong_width = rect_line(v, (6.0, 10.0, 10.0, 15.0));
    plane.add(wrong_width).unwrap();

    let neighbors = line.find_neighbors(&plane, laparams.line_margin).unwrap();
    assert_count_equal(&neighbors, &[line, bottom_aligned_right, top_aligned_left, centrally_aligned_overlapping]);
}
