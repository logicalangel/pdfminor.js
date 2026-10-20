//! `PDFPageInterpreter`, `PDFTextDevice` and `TagExtractor` against Python: the calls the
//! interpreter makes on a recording device, the characters a text device positions, and the
//! TagExtractor output, on the upstream samples and tests/corpus (reference data from
//! `tools/gen_testdata/interp.py`).
//!
//! `INTERP_DIFF_DIR=<dir>` compares the references in another directory instead (e.g. fuzzed
//! files: `uv run python tools/gen_testdata/interp.py --corpus tests/out/fuzz --out <dir>`);
//! `INTERP_FILTER=<substring>` limits the files.

use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use pdfminer::pdfcolor::PDFColorSpace;
use pdfminer::pdfdevice::{PDFDevice, PDFTextDevice, TagExtractor, text_render_string};
use pdfminer::pdfdocument::PDFDocument;
use pdfminer::pdffont::PDFFont;
use pdfminer::pdfinterp::{
    PDFGraphicState, PDFGraphicStateRef, PDFPageInterpreter, PDFResourceManager, PDFTextState, dash_repr,
};
use pdfminer::pdfpage::PDFPage;
use pdfminer::pdfparser::PDFParser;
use pdfminer::pdftypes::PDFStream;
use pdfminer::psparser::PSLiteral;
use pdfminer::pyrt::error::PyResult;
use pdfminer::pyrt::fmt::float_repr;
use pdfminer::pyrt::io::BytesIO;
use pdfminer::pyrt::textio::BytesOut;
use pdfminer::pyrt::value::{Value, dict_repr};
use pdfminer::utils::{Matrix, PathSegment};
use serde_json::Value as J;
use sha2::{Digest, Sha256};

const MAX_PAGES: usize = 3;
const MAX_LINES: usize = 3000;
const PASSWORDS: [&str; 5] = ["", "foo", "baz", "owner", "user"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|c| format!("{c:02x}")).collect()
}

fn f(x: f64) -> String {
    float_repr(x)
}

fn mat(m: &[f64]) -> String {
    format!("({})", m.iter().map(|x| f(*x)).collect::<Vec<_>>().join(", "))
}

fn m6(m: Matrix) -> String {
    mat(&[m.0, m.1, m.2, m.3, m.4, m.5])
}

fn items(seq: &[Value]) -> String {
    format!("[{}]", seq.iter().map(|v| v.repr()).collect::<Vec<_>>().join(", "))
}

fn cs(c: &PDFColorSpace) -> String {
    format!("{}/{}", c.name(), c.ncomponents().repr())
}

fn gstate(gs: &PDFGraphicState) -> String {
    format!(
        "lw={} cap={} join={} miter={} dash={} intent={} flat={} sc={} scs={} nc={} ncs={}",
        gs.linewidth.repr(),
        gs.linecap.repr(),
        gs.linejoin.repr(),
        gs.miterlimit.repr(),
        dash_repr(&gs.dash),
        gs.intent.repr(),
        gs.flatness.repr(),
        gs.scolor.repr(),
        cs(&gs.scs),
        gs.ncolor.repr(),
        cs(&gs.ncs),
    )
}

fn path_str(path: &[PathSegment]) -> String {
    path.iter()
        .map(|s| {
            let mut parts = vec![s.op.to_string()];
            parts.extend(s.args.iter().map(|a| f(*a)));
            parts.join(",")
        })
        .collect::<Vec<_>>()
        .join(";")
}

fn props_repr(p: Option<&Value>) -> String {
    p.map_or("None".to_string(), |v| v.repr())
}

type Log = Rc<RefCell<Vec<String>>>;

struct Recorder {
    ctm: Option<Matrix>,
    log: Log,
}

impl Recorder {
    fn push(&self, s: String) {
        self.log.borrow_mut().push(s);
    }
}

impl PDFDevice for Recorder {
    fn ctm(&self) -> Option<Matrix> {
        self.ctm
    }
    fn set_ctm(&mut self, ctm: Matrix) {
        self.ctm = Some(ctm);
        self.push(format!("ctm {}", m6(ctm)));
    }
    fn begin_tag(&mut self, tag: &PSLiteral, props: Option<&Value>) -> PyResult<()> {
        self.push(format!("begin_tag {} {}", tag.repr(), props_repr(props)));
        Ok(())
    }
    fn end_tag(&mut self) -> PyResult<()> {
        self.push("end_tag".into());
        Ok(())
    }
    fn do_tag(&mut self, tag: &PSLiteral, props: Option<&Value>) -> PyResult<()> {
        self.push(format!("do_tag {} {}", tag.repr(), props_repr(props)));
        Ok(())
    }
    fn begin_page(&mut self, page: &PDFPage, ctm: Matrix) -> PyResult<()> {
        let mb = page.mediabox;
        self.push(format!("begin_page {} {} {}", mat(&[mb.0, mb.1, mb.2, mb.3]), page.rotate, m6(ctm)));
        Ok(())
    }
    fn end_page(&mut self, _page: &PDFPage) -> PyResult<()> {
        self.push("end_page".into());
        Ok(())
    }
    fn begin_figure(&mut self, name: &str, bbox: &[Value], matrix: &[Value]) -> PyResult<()> {
        self.push(format!("begin_figure {name} {} {}", items(bbox), items(matrix)));
        Ok(())
    }
    fn end_figure(&mut self, name: &str) -> PyResult<()> {
        self.push(format!("end_figure {name}"));
        Ok(())
    }
    fn paint_path(
        &mut self,
        gs: &PDFGraphicStateRef,
        stroke: bool,
        fill: bool,
        evenodd: bool,
        path: &[PathSegment],
    ) -> PyResult<()> {
        self.push(format!(
            "paint {}{}{} {} {}",
            stroke as i32,
            fill as i32,
            evenodd as i32,
            path_str(path),
            gstate(&gs.borrow())
        ));
        Ok(())
    }
    fn render_image(&mut self, name: &str, stream: &Rc<PDFStream>) -> PyResult<()> {
        let raw = stream.get_rawdata();
        let sha = hex(&Sha256::digest(raw.as_deref().unwrap_or(b"")))[..16].to_string();
        self.push(format!(
            "image {name} {} {} {sha}",
            stream.objid.get().map_or("None".to_string(), |o| o.to_string()),
            dict_repr(&stream.attrs.borrow())
        ));
        Ok(())
    }
    fn render_string(
        &mut self,
        ts: &mut PDFTextState,
        seq: &[Value],
        ncs: &PDFColorSpace,
        gs: &PDFGraphicStateRef,
    ) -> PyResult<()> {
        self.push(format!(
            "str {} {} {} {} {} {} {} {} {} {} {} {} {}",
            ts.font.as_ref().map_or("None".to_string(), |f| f.repr()),
            f(ts.fontsize),
            f(ts.charspace),
            f(ts.wordspace),
            f(ts.scaling),
            f(ts.leading),
            ts.render.repr(),
            f(ts.rise),
            m6(ts.matrix),
            mat(&[ts.linematrix.0, ts.linematrix.1]),
            items(seq),
            cs(ncs),
            gstate(&gs.borrow())
        ));
        Ok(())
    }
}

struct TextRecorder {
    ctm: Option<Matrix>,
    log: Log,
}

impl PDFDevice for TextRecorder {
    fn ctm(&self) -> Option<Matrix> {
        self.ctm
    }
    fn set_ctm(&mut self, ctm: Matrix) {
        self.ctm = Some(ctm);
    }
    fn render_string(
        &mut self,
        ts: &mut PDFTextState,
        seq: &[Value],
        ncs: &PDFColorSpace,
        gs: &PDFGraphicStateRef,
    ) -> PyResult<()> {
        text_render_string(self, ts, seq, ncs, gs)?;
        self.log.borrow_mut().push(format!("lm {}", mat(&[ts.linematrix.0, ts.linematrix.1])));
        Ok(())
    }
}

impl PDFTextDevice for TextRecorder {
    fn render_char(
        &mut self,
        matrix: Matrix,
        font: &Rc<PDFFont>,
        fontsize: f64,
        scaling: f64,
        rise: f64,
        cid: i64,
        ncs: &PDFColorSpace,
        _gs: &PDFGraphicStateRef,
    ) -> PyResult<f64> {
        let adv = font.char_width(cid)? * fontsize * scaling;
        self.log.borrow_mut().push(format!(
            "char {} {cid} {} {} {} {} {}",
            m6(matrix),
            f(fontsize),
            f(scaling),
            f(rise),
            ncs.name(),
            f(adv)
        ));
        Ok(adv)
    }
}

fn open_doc(data: &[u8]) -> Result<Rc<PDFDocument>, String> {
    let mut err = None;
    for password in PASSWORDS {
        let parser = PDFParser::new(BytesIO::new(Rc::from(data)));
        match PDFDocument::new(parser, password, true, true) {
            Ok(d) => return Ok(d),
            Err(e) => {
                if err.is_none() {
                    err = Some(e.kind.name().to_string());
                }
            }
        }
    }
    Err(err.unwrap())
}

fn run(data: &[u8], kind: &str) -> J {
    let log: Log = Rc::new(RefCell::new(Vec::new()));
    let doc = match open_doc(data) {
        Ok(d) => d,
        Err(e) => return serde_json::json!({ "open": e }),
    };
    let rsrcmgr = PDFResourceManager::new(true);
    let out = Rc::new(RefCell::new(BytesOut::new()));
    let device: Rc<RefCell<dyn PDFDevice>> = match kind {
        "dev" => Rc::new(RefCell::new(Recorder { ctm: None, log: log.clone() })),
        "text" => Rc::new(RefCell::new(TextRecorder { ctm: None, log: log.clone() })),
        _ => Rc::new(RefCell::new(TagExtractor::new(out.clone(), "utf-8"))),
    };
    let mut interp = PDFPageInterpreter::new(rsrcmgr, device);
    let mut pages = PDFPage::create_pages(&doc);
    for _ in 0..MAX_PAGES {
        let page = match pages.next() {
            None => break,
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                log.borrow_mut().push(format!("pages error {}", e.kind.name()));
                break;
            }
        };
        match interp.process_page(&page) {
            Ok(()) => log.borrow_mut().push("end".into()),
            Err(e) => log.borrow_mut().push(format!("error {}", e.kind.name())),
        }
    }
    let mut res = serde_json::Map::new();
    if kind == "tag" {
        let b = out.borrow().getvalue().to_vec();
        let o = if b.len() <= 4096 { hex(&b) } else { format!("sha256:{}:{}", hex(&Sha256::digest(&b)), b.len()) };
        res.insert("out".into(), J::String(o));
    }
    let log = log.borrow();
    let whole = log.join("\n");
    res.insert("n".into(), J::from(log.len()));
    res.insert("sha".into(), J::String(hex(&Sha256::digest(whole.as_bytes()))));
    res.insert("log".into(), J::Array(log.iter().take(MAX_LINES).map(|s| J::String(s.clone())).collect()));
    J::Object(res)
}

fn first_diff(want: &J, got: &J) -> Option<String> {
    let mut want = want.clone();
    let mut got = got.clone();
    if want.get("out_diverges").is_some() {
        // DIVERGE(charset-normalizer): the TagExtractor bytes may differ.
        for x in [&mut want, &mut got] {
            if let Some(o) = x.as_object_mut() {
                o.remove("out");
                o.remove("out_diverges");
            }
        }
    }
    if want == got {
        return None;
    }
    if let (Some(w), Some(g)) = (want.get("log").and_then(|l| l.as_array()), got.get("log").and_then(|l| l.as_array()))
    {
        for (i, (a, b)) in w.iter().zip(g.iter()).enumerate() {
            if a != b {
                return Some(format!("line {i}:\n  py: {a}\n  rs: {b}"));
            }
        }
        if w.len() != g.len() {
            return Some(format!(
                "length py {} rs {}; next py: {:?} rs: {:?}",
                w.len(),
                g.len(),
                w.get(g.len()),
                g.get(w.len())
            ));
        }
    }
    for k in ["open", "out", "n", "sha"] {
        if want.get(k) != got.get(k) {
            return Some(format!("{k}: py {:?} rs {:?}", want.get(k), got.get(k)));
        }
    }
    Some("differs".into())
}

fn check_dir(dir: &Path) -> (usize, Vec<String>) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".json.gz"))
        .collect();
    files.sort();
    let filter = std::env::var("INTERP_FILTER").ok();
    let mut failures = Vec::new();
    let mut n = 0;
    for file in files {
        if let Some(flt) = &filter
            && !file.to_string_lossy().contains(flt.as_str())
        {
            continue;
        }
        let mut s = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&file).unwrap()).read_to_string(&mut s).unwrap();
        let rec: J = serde_json::from_str(&s).unwrap();
        let path = rec["path"].as_str().unwrap();
        let pdf = if Path::new(path).is_absolute() { PathBuf::from(path) } else { root().join(path) };
        let data = std::fs::read(&pdf).unwrap_or_else(|e| panic!("{}: {e}", pdf.display()));
        for kind in ["dev", "text", "tag"] {
            n += 1;
            let got = run(&data, kind);
            if let Some(d) = first_diff(&rec["runs"][kind], &got) {
                failures.push(format!("{} [{kind}]: {d}", file.file_name().unwrap().to_string_lossy()));
            }
        }
    }
    (n, failures)
}

#[test]
fn interp_matches_python() {
    let dir = match std::env::var("INTERP_DIFF_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/interp"),
    };
    let (n, failures) = check_dir(&dir);
    for f in &failures {
        eprintln!("{f}\n");
    }
    assert!(failures.is_empty(), "{} of {n} runs differ", failures.len());
    eprintln!("{n} runs match");
}
