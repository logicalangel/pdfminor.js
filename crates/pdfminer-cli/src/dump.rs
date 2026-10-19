//! `pdfminer-rs dump FILE [--laparams default|none|JSON] [--password PW]`: the canonical layout
//! dump of `tools/oracle.py layout` (not an upstream tool; the serialization is
//! `pdfminer::dump`).

use std::io::Write;

use pdfminer::high_level::read_file;
use pdfminer::layout::LAParams;
use pdfminer::pyrt::error::{ExcKind, PyErr, PyResult};
use pdfminer::pyrt::value::Value;

use crate::{Stdout, die};

/// `laparams_from(spec)`: `LAParams(**json.loads(spec))`.
pub fn laparams_from(spec: Option<&str>) -> PyResult<Option<LAParams>> {
    let spec = match spec {
        None | Some("default") => return Ok(Some(LAParams::default())),
        Some("none") => return Ok(None),
        Some(s) => s,
    };
    let json: serde_json::Value =
        serde_json::from_str(spec).map_err(|e| PyErr::new(ExcKind::ValueError, format!("Expecting value: {e}")))?;
    let serde_json::Value::Object(map) = json else {
        return Err(PyErr::new(ExcKind::TypeError, "argument after ** must be a mapping"));
    };
    let mut la = LAParams::default();
    let num = |k: &str, v: &serde_json::Value| -> PyResult<f64> {
        v.as_f64().ok_or_else(|| PyErr::new(ExcKind::TypeError, format!("LAParams {k} must be a number")))
    };
    let truthy = |v: &serde_json::Value| -> bool {
        match v {
            serde_json::Value::Null => false,
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Number(n) => n.as_f64() != Some(0.0),
            serde_json::Value::String(s) => !s.is_empty(),
            serde_json::Value::Array(a) => !a.is_empty(),
            serde_json::Value::Object(o) => !o.is_empty(),
        }
    };
    for (k, v) in &map {
        match k.as_str() {
            "line_overlap" => la.line_overlap = num(k, v)?,
            "char_margin" => la.char_margin = num(k, v)?,
            "line_margin" => la.line_margin = num(k, v)?,
            "word_margin" => la.word_margin = num(k, v)?,
            "boxes_flow" => {
                let val = match v {
                    serde_json::Value::Null => Value::None,
                    serde_json::Value::Number(n) => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
                    serde_json::Value::Bool(b) => Value::Bool(*b),
                    _ => Value::str(&v.to_string()),
                };
                la.boxes_flow = LAParams::boxes_flow_from_value(&val)?;
            }
            "detect_vertical" => la.detect_vertical = truthy(v),
            "all_texts" => la.all_texts = truthy(v),
            _ => {
                return Err(PyErr::new(
                    ExcKind::TypeError,
                    format!("LAParams.__init__() got an unexpected keyword argument '{k}'"),
                ));
            }
        }
    }
    la._validate()?;
    Ok(Some(la))
}

pub fn main(args: &[String]) -> i32 {
    let mut stdout = Stdout::new();
    let mut file: Option<String> = None;
    let mut laparams: Option<String> = None;
    let mut password = String::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(v) = a.strip_prefix("--laparams=") {
            laparams = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("--password=") {
            password = v.to_string();
        } else if a == "--laparams" || a == "--password" {
            let Some(v) = args.get(i + 1) else {
                eprintln!("pdfminer-rs dump: error: argument {a}: expected one argument");
                return 2;
            };
            if a == "--laparams" {
                laparams = Some(v.clone());
            } else {
                password = v.clone();
            }
            i += 1;
        } else if file.is_none() {
            file = Some(a.clone());
        } else {
            eprintln!("pdfminer-rs dump: error: unrecognized arguments: {a}");
            return 2;
        }
        i += 1;
    }
    let Some(file) = file else {
        eprintln!("usage: pdfminer-rs dump FILE [--laparams default|none|JSON] [--password PW]");
        return 2;
    };

    let laparams = match laparams_from(laparams.as_deref()) {
        Ok(l) => l,
        Err(e) => die(&mut stdout, &e),
    };
    let data = match read_file(&file) {
        Ok(d) => d,
        Err(e) => {
            let _ = writeln!(stdout, "{}", pdfminer::dump::status_line(&Err(e)));
            let _ = stdout.flush();
            return 0;
        }
    };
    let mut w = stdout.clone();
    let _ = pdfminer::dump::dump_layout(data, laparams, &password, &mut |s: &str| w.write_all(s.as_bytes()));
    let _ = stdout.flush();
    0
}
