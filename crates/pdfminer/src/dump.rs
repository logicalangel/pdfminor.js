//! The canonical layout dump: not an upstream module.
//!
//! `tools/oracle.py layout` prints every page `extract_pages` yields as one line of JSON, with
//! every attribute of every layout object, then a status line; `pdfminer-rs dump` prints the
//! same bytes through this module, which is how the difftests compare layout trees. The Node
//! binding reuses it. The format rules are in `tools/oracle.py`'s docstring:
//!
//! - geometry (bbox, matrix, pts, size, adv, linewidth, color components) is converted with
//!   `float()` first;
//! - other values keep their Python types, encoded by `enc`: int, float, str, bytes ->
//!   `{"bytes": hex}`, literal -> `{"lit": name}`, keyword -> `{"kwd": hex}`, list/tuple -> list,
//!   dict -> `{"dict": [[k, v], ...]}`, ref -> `{"ref": objid}`, stream -> `{"stream": objid}`;
//! - JSON as `json.dumps(o, ensure_ascii=True, separators=(",", ":"))`: floats as `repr`,
//!   `NaN`, `Infinity`, `-Infinity`; lone surrogates as `\udXXX`;
//! - the last line is `{"status":"ok"}` or `{"status":"error","exc":Class,"msg":str(e)}`.

use std::fmt::Write as _;
use std::rc::Rc;

use crate::high_level::{ExtractPages, FileOrName, extract_pages};
use crate::layout::{LAParams, LTItem};
use crate::pdfinterp::{Color, PDFGraphicState, StandardColor};
use crate::psparser::NameType;
use crate::pyrt::error::{PyErr, PyResult};
use crate::pyrt::fmt::float_repr;
use crate::pyrt::pystr::{PyStr, json_escape_cp};
use crate::pyrt::value::{Value, py_float};
use crate::utils::{Matrix, Point};

/// A dump value: what `tools/oracle.py` builds before `json.dumps`. The JSON writer and the
/// Node binding both consume it, so they cannot drift apart.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(PyStr),
    List(Vec<Node>),
    /// An object, its keys in Python's insertion order.
    Object(Vec<(&'static str, Node)>),
}

impl Node {
    fn str(s: &str) -> Node {
        Node::Str(PyStr::from(s))
    }

    fn floats(xs: &[f64]) -> Node {
        Node::List(xs.iter().map(|&x| Node::Float(x)).collect())
    }

    /// `json.dumps(node, ensure_ascii=True, separators=(",", ":"))`.
    pub fn write_json(&self, out: &mut String) {
        match self {
            Node::Null => out.push_str("null"),
            Node::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Node::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Node::Float(x) => json_float(*x, out),
            Node::Str(s) => s.json_ascii(out),
            Node::List(items) => {
                out.push('[');
                for (i, x) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    x.write_json(out);
                }
                out.push(']');
            }
            Node::Object(items) => {
                out.push('{');
                for (i, (k, x)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    json_str(k, out);
                    out.push(':');
                    x.write_json(out);
                }
                out.push('}');
            }
        }
    }

    /// The JSON text.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }
}

/// A JSON float as `json.dumps` writes it.
pub fn json_float(x: f64, out: &mut String) {
    if x.is_nan() {
        out.push_str("NaN");
    } else if x.is_infinite() {
        out.push_str(if x > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        out.push_str(&float_repr(x));
    }
}

/// A JSON string (`ensure_ascii=True`).
pub fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        json_escape_cp(c as u32, out);
    }
    out.push('"');
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

fn opt_int(v: Option<i64>) -> Node {
    v.map_or(Node::Null, Node::Int)
}

fn bytes_node(b: &[u8]) -> Node {
    Node::Object(vec![("bytes", Node::str(&hex(b)))])
}

/// `enc(v)` of `tools/oracle.py`.
pub fn enc(v: &Value) -> Node {
    match v {
        Value::None => Node::Null,
        Value::Bool(b) => Node::Bool(*b),
        Value::Int(i) => Node::Int(*i),
        Value::Float(f) => Node::Float(*f),
        Value::Str(s) => Node::str(s),
        Value::Bytes(b) => bytes_node(b),
        Value::Literal(l) => Node::Object(vec![(
            "lit",
            match &l.name {
                NameType::Str(s) => Node::str(s),
                NameType::Bytes(b) => bytes_node(b),
            },
        )]),
        Value::Keyword(k) => Node::Object(vec![("kwd", Node::str(&hex(&k.name)))]),
        Value::List(l) => Node::List(l.borrow().iter().map(enc).collect()),
        Value::Tuple(t) => Node::List(t.iter().map(enc).collect()),
        Value::Dict(d) => Node::Object(vec![(
            "dict",
            Node::List(d.borrow().iter().map(|(k, x)| Node::List(vec![Node::str(k), enc(x)])).collect()),
        )]),
        Value::Ref(r) => Node::Object(vec![("ref", Node::Int(r.objid()))]),
        Value::Stream(s) => Node::Object(vec![("stream", opt_int(s.objid.get()))]),
    }
}

fn point(p: Point) -> Node {
    Node::floats(&[p.0, p.1])
}

fn standard_color(c: &StandardColor) -> Node {
    match c {
        StandardColor::IntGray(g) => Node::Float(*g as f64),
        StandardColor::Gray(g) => Node::Float(*g),
        StandardColor::Rgb(r, g, b) => Node::floats(&[*r, *g, *b]),
        StandardColor::Cmyk(c, m, y, k) => Node::floats(&[*c, *m, *y, *k]),
    }
}

/// `color(c)`: gray number, (r, g, b), (c, m, y, k), pattern name, or (base, pattern name).
fn color(c: Option<&Color>) -> Node {
    match c {
        None => Node::Null,
        Some(Color::Standard(s)) => standard_color(s),
        Some(Color::Pattern(name)) => Node::Object(vec![("pattern", Node::str(name))]),
        Some(Color::UncoloredPattern(base, name)) => {
            Node::Object(vec![("base", standard_color(base)), ("pattern", Node::str(name))])
        }
    }
}

fn dash(d: &Option<(Value, Value)>) -> Node {
    match d {
        None => Node::Null,
        Some((a, b)) => Node::List(vec![enc(a), enc(b)]),
    }
}

fn graphicstate(gs: &PDFGraphicState) -> PyResult<Node> {
    Ok(Node::Object(vec![
        ("linewidth", Node::Float(py_float(&gs.linewidth)?)),
        ("linecap", enc(&gs.linecap)),
        ("linejoin", enc(&gs.linejoin)),
        ("miterlimit", enc(&gs.miterlimit)),
        ("dash", dash(&gs.dash)),
        ("intent", enc(&gs.intent)),
        ("flatness", enc(&gs.flatness)),
        ("scolor", color(Some(&gs.scolor))),
        ("scs", Node::str(gs.scs.name())),
        ("ncolor", color(Some(&gs.ncolor))),
        ("ncs", Node::str(gs.ncs.name())),
    ]))
}

fn bbox(o: &LTItem) -> Node {
    let (x0, y0, x1, y1) = o.bbox();
    Node::floats(&[x0, y0, x1, y1])
}

fn matrix(m: Matrix) -> Node {
    Node::floats(&[m.0, m.1, m.2, m.3, m.4, m.5])
}

/// `item(o)`: one layout object and its children.
pub fn item(o: &LTItem) -> PyResult<Node> {
    let mut d: Vec<(&'static str, Node)> = vec![("type", Node::str(o.class_name()))];
    if o.is_ltcomponent() {
        d.push(("bbox", bbox(o)));
    }
    if let Some(c) = o.as_char() {
        d.push(("text", Node::Str(o.get_text())));
        d.push(("fontname", enc(&c.fontname)));
        d.push(("size", Node::Float(c.size)));
        d.push(("adv", Node::Float(c.adv)));
        d.push(("upright", Node::Bool(c.upright)));
        d.push(("matrix", matrix(c.matrix)));
        d.push(("ncs", Node::str(c.ncs.name())));
        d.push(("graphicstate", graphicstate(&c.graphicstate.borrow())?));
    } else if o.is_ltanno() {
        d.push(("text", Node::Str(o.get_text())));
    } else if let Some(c) = o.as_curve() {
        d.push(("pts", Node::List(c.pts.iter().map(|p| point(*p)).collect())));
        d.push(("linewidth", Node::Float(py_float(&c.linewidth)?)));
        d.push(("stroke", Node::Bool(c.stroke)));
        d.push(("fill", Node::Bool(c.fill)));
        d.push(("evenodd", Node::Bool(c.evenodd)));
        d.push(("stroking_color", color(c.stroking_color.as_ref())));
        d.push(("non_stroking_color", color(c.non_stroking_color.as_ref())));
        let path = c.original_path.as_ref().map_or(Node::Null, |p| {
            Node::List(
                p.iter()
                    .map(|(op, pts)| {
                        let mut seg = vec![Node::str(op)];
                        seg.extend(pts.iter().map(|pt| point(*pt)));
                        Node::List(seg)
                    })
                    .collect(),
            )
        });
        d.push(("original_path", path));
        d.push(("dashing_style", dash(&c.dashing_style)));
    } else if let Some(i) = o.as_image() {
        d.push(("name", Node::str(&i.name)));
        d.push(("srcsize", Node::List(vec![enc(&i.srcsize.0), enc(&i.srcsize.1)])));
        d.push(("imagemask", enc(&i.imagemask)));
        d.push(("bits", enc(&i.bits)));
        d.push(("colorspace", enc(&i.colorspace)));
        d.push(("stream", Node::Object(vec![("stream", opt_int(i.stream.objid.get()))])));
    }
    if o.is_lttextbox() {
        d.push(("index", Node::Int(o.index())));
    }
    if let Some(f) = o.as_figure() {
        d.push(("name", Node::str(&f.name)));
        d.push(("matrix", matrix(f.matrix)));
    }
    if let Some(p) = o.as_page() {
        d.push(("pageid", Node::Int(p.pageid)));
        d.push(("rotate", Node::Float(py_float(&p.rotate)?)));
        d.push(("groups", o.groups().map_or(Node::Null, |g| Node::List(g.iter().map(group).collect()))));
    }
    if o.is_ltcontainer() {
        let items = o.objs_ref().iter().map(item).collect::<PyResult<Vec<_>>>()?;
        d.push(("items", Node::List(items)));
    }
    Ok(Node::Object(d))
}

/// `group(g)`: text groups refer to the text boxes of the page by index.
pub fn group(g: &LTItem) -> Node {
    let mut d: Vec<(&'static str, Node)> = vec![("type", Node::str(g.class_name()))];
    if g.is_lttextgroup() {
        d.push(("bbox", bbox(g)));
        d.push(("items", Node::List(g.objs_ref().iter().map(group).collect())));
    } else {
        d.push(("index", Node::Int(g.index())));
    }
    Node::Object(d)
}

/// One page line (without the newline).
pub fn dump_page(page: &LTItem) -> PyResult<String> {
    Ok(item(page)?.to_json())
}

/// The final status line (without the newline).
pub fn status_line(r: &PyResult<()>) -> String {
    match r {
        Ok(()) => "{\"status\":\"ok\"}".to_string(),
        Err(e) => error_line(e),
    }
}

fn error_line(e: &PyErr) -> String {
    Node::Object(vec![("status", Node::str("error")), ("exc", Node::str(e.kind.name())), ("msg", Node::str(&e.msg))])
        .to_json()
}

/// `cmd_layout`: the whole dump of a PDF, written line by line to `write` (`laparams` `None`
/// runs the aggregator without analysis, like `--laparams none`).
pub fn dump_layout(
    data: Rc<[u8]>,
    laparams: Option<LAParams>,
    password: &str,
    write: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let pages: ExtractPages = match laparams {
        None => ExtractPages::without_laparams(data, password),
        Some(l) => extract_pages(FileOrName::Data(data), password, None, 0, true, Some(l)),
    };
    let mut r: PyResult<()> = Ok(());
    for page in pages {
        match page.and_then(|p| dump_page(&p)) {
            Ok(line) => {
                write(&line)?;
                write("\n")?;
            }
            Err(e) => {
                r = Err(e);
                break;
            }
        }
    }
    write(&status_line(&r))?;
    write("\n")
}
