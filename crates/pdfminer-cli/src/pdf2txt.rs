//! Port of `tools/pdf2txt.py`.
//!
//! A command line tool for extracting text and images from PDF and output it to plain text,
//! html, xml or tags.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Write;
use std::rc::Rc;

use pdfminer::high_level::{ExtractTextToFp, extract_text_to_fp, read_file};
use pdfminer::layout::LAParams;
use pdfminer::pyrt::PyStr;
use pdfminer::pyrt::error::{ExcKind, PyErr, PyResult};
use pdfminer::pyrt::textio::{AnyIO, BinaryFile, TextFile, os_error, os_error_path, stdout_encoding};
use pdfminer::pyrt::value::{Value, py_float, py_int};

use crate::argparse::{ArgVal, ArgumentParser, ConvError, Kind, Namespace, Nargs};
use crate::{Stdout, die};

const DOC: &str =
    "A command line tool for extracting text and images from PDF and\noutput it to plain text, html, xml or tags.\n";

const OUTPUT_TYPES: [(&str, &str); 4] = [(".htm", "html"), (".html", "html"), (".xml", "xml"), (".tag", "tag")];

/// `float_or_disabled(x)`.
fn float_or_disabled(x: &str) -> Result<ArgVal, ConvError> {
    if PyStr::from(x.to_lowercase()).strip().as_str() == Some("disabled") {
        return Ok(ArgVal::None);
    }
    match py_float(&Value::str(x)) {
        Ok(f) => Ok(ArgVal::Float(f)),
        Err(_) => Err(ConvError::Msg(format!("invalid float value: {x}"))),
    }
}

/// `int(x)` as argparse's `type=int`.
pub fn conv_int(x: &str) -> Result<ArgVal, ConvError> {
    py_int(&Value::str(x)).map(ArgVal::Int).map_err(|_| ConvError::Invalid)
}

/// `float(x)` as argparse's `type=float`.
pub fn conv_float(x: &str) -> Result<ArgVal, ConvError> {
    py_float(&Value::str(x)).map(ArgVal::Float).map_err(|_| ConvError::Invalid)
}

pub fn create_parser() -> ArgumentParser {
    let mut parser = ArgumentParser::new("pdf2txt.py", Some(DOC));
    parser.version = format!("pdfminer.six v{}", pdfminer::UPSTREAM_VERSION);
    let p = &mut parser;
    let (store, store_true, one, zero, plus) = (Kind::Store, Kind::StoreTrue, Nargs::One, Nargs::Zero, Nargs::Plus);
    p.add_argument(0, &["files"], store, plus, None, ArgVal::None, "One or more paths to PDF files.");

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
    p.add_argument(
        1,
        &["--disable-caching", "-C"],
        store_true,
        zero,
        None,
        ArgVal::Bool(false),
        "If caching or resources, such as fonts, should be disabled.",
    );

    let parse_params = p.add_argument_group("Parser", Some("Used during PDF parsing"));
    p.add_argument(
        parse_params,
        &["--page-numbers"],
        store,
        plus,
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
        &["--maxpages", "-m"],
        store,
        one,
        Some((conv_int, "int")),
        ArgVal::Int(0),
        "The maximum number of pages to parse.",
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
    p.add_argument(
        parse_params,
        &["--rotation", "-R"],
        store,
        one,
        Some((conv_int, "int")),
        ArgVal::Int(0),
        "The number of degrees to rotate the PDF before other types of processing.",
    );

    let la_params = LAParams::default(); // will be used for defaults
    let la = p.add_argument_group("Layout analysis", Some("Used during layout analysis."));
    p.add_argument(
        la,
        &["--no-laparams", "-n"],
        store_true,
        zero,
        None,
        ArgVal::Bool(false),
        "If layout analysis parameters should be ignored.",
    );
    p.add_argument(
        la,
        &["--detect-vertical", "-V"],
        store_true,
        zero,
        None,
        ArgVal::Bool(la_params.detect_vertical),
        "If vertical text should be considered during layout analysis",
    );
    p.add_argument(
        la,
        &["--line-overlap"],
        store,
        one,
        Some((conv_float, "float")),
        ArgVal::Float(la_params.line_overlap),
        "If two characters have more overlap than this they are considered to be on the same line. The overlap is specified relative to the minimum height of both characters.",
    );
    p.add_argument(
        la,
        &["--char-margin", "-M"],
        store,
        one,
        Some((conv_float, "float")),
        ArgVal::Float(la_params.char_margin),
        "If two characters are closer together than this margin they are considered to be part of the same line. The margin is specified relative to the width of the character.",
    );
    p.add_argument(
        la,
        &["--word-margin", "-W"],
        store,
        one,
        Some((conv_float, "float")),
        ArgVal::Float(la_params.word_margin),
        "If two characters on the same line are further apart than this margin then they are considered to be two separate words, and an intermediate space will be added for readability. The margin is specified relative to the width of the character.",
    );
    p.add_argument(
        la,
        &["--line-margin", "-L"],
        store,
        one,
        Some((conv_float, "float")),
        ArgVal::Float(la_params.line_margin),
        "If two lines are close together they are considered to be part of the same paragraph. The margin is specified relative to the height of a line.",
    );
    p.add_argument(
        la,
        &["--boxes-flow", "-F"],
        store,
        one,
        Some((float_or_disabled, "float_or_disabled")),
        la_params.boxes_flow.map_or(ArgVal::None, ArgVal::Float),
        "Specifies how much a horizontal and vertical position of a text matters when determining the order of lines. The value should be within the range of -1.0 (only horizontal position matters) to +1.0 (only vertical position matters). You can also pass `disabled` to disable advanced layout analysis, and instead return text based on the position of the bottom left corner of the text box.",
    );
    p.add_argument(
        la,
        &["--all-texts", "-A"],
        store_true,
        zero,
        None,
        ArgVal::Bool(la_params.all_texts),
        "If layout analysis should be performed on text in figures.",
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
    p.add_argument(
        out,
        &["--output_type", "-t"],
        store,
        one,
        None,
        ArgVal::Str("text".into()),
        "Type of output to generate {text,html,xml,tag}.",
    );
    p.add_argument(
        out,
        &["--codec", "-c"],
        store,
        one,
        None,
        ArgVal::Str("utf-8".into()),
        "Text encoding to use in output file.",
    );
    p.add_argument(
        out,
        &["--output-dir", "-O"],
        store,
        one,
        None,
        ArgVal::None,
        "The output directory to put extracted images in. If not given, images are not extracted.",
    );
    p.add_argument(
        out,
        &["--layoutmode", "-Y"],
        store,
        one,
        None,
        ArgVal::Str("normal".into()),
        "Type of layout to use when generating html {normal,exact,loose}. If normal,each line is positioned separately in the html. If exact, each character is positioned separately in the html. If loose, same result as normal but with an additional newline after each text line. Only used when output_type is html.",
    );
    p.add_argument(
        out,
        &["--scale", "-s"],
        store,
        one,
        Some((conv_float, "float")),
        ArgVal::Float(1.0),
        "The amount of zoom to use when generating html file. Only used when output_type is html.",
    );
    p.add_argument(
        out,
        &["--strip-control", "-S"],
        store_true,
        zero,
        None,
        ArgVal::Bool(false),
        "Remove control statement from text. Only used when output_type is xml.",
    );

    parser
}

pub(crate) fn get_str(ns: &Namespace, k: &str) -> Option<String> {
    match ns.get(k) {
        Some(ArgVal::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

pub(crate) fn get_bool(ns: &Namespace, k: &str) -> bool {
    matches!(ns.get(k), Some(ArgVal::Bool(true)))
}

fn get_f64(ns: &Namespace, k: &str) -> f64 {
    match ns.get(k) {
        Some(ArgVal::Float(f)) => *f,
        Some(ArgVal::Int(i)) => *i as f64,
        _ => 0.0,
    }
}

fn get_i64(ns: &Namespace, k: &str) -> i64 {
    match ns.get(k) {
        Some(ArgVal::Int(i)) => *i,
        _ => 0,
    }
}

/// `files` (and other `nargs="+"` strings).
pub(crate) fn get_strs(ns: &Namespace, k: &str) -> Vec<String> {
    match ns.get(k) {
        Some(ArgVal::List(l)) => l
            .iter()
            .map(|v| match v {
                ArgVal::Str(s) => s.clone(),
                _ => String::new(),
            })
            .collect(),
        _ => vec![],
    }
}

/// `--page-numbers` (1-based) as a set of 0-based page numbers, or `None` when not given.
pub(crate) fn page_numbers_arg(ns: &Namespace) -> Option<HashSet<i64>> {
    match ns.get("page_numbers") {
        Some(ArgVal::List(l)) if !l.is_empty() => Some(
            l.iter()
                .map(|v| match v {
                    ArgVal::Int(i) => i.saturating_sub(1),
                    _ => 0,
                })
                .collect(),
        ),
        _ => None,
    }
}

/// `{int(x) - 1 for x in pagenos.split(",")}`.
pub(crate) fn pagenos_arg(pagenos: &str) -> PyResult<HashSet<i64>> {
    pagenos.split(',').map(|x| py_int(&Value::str(x)).map(|i| i.saturating_sub(1))).collect()
}

/// The parsed arguments (`parse_args(args)`), with the fields `extract_text` uses.
pub struct Args {
    pub files: Vec<String>,
    pub outfile: String,
    pub output_type: String,
    pub fp_args: ExtractTextToFp,
}

/// `parse_args(args)` after argparse: `Err` is an exception (a traceback).
fn finish_args(ns: Namespace) -> PyResult<Args> {
    // Propagate parsed layout parameters to LAParams object
    let laparams = if get_bool(&ns, "no_laparams") {
        None
    } else {
        let boxes_flow = match ns.get("boxes_flow") {
            Some(ArgVal::Float(f)) => Some(*f),
            _ => None,
        };
        Some(LAParams::new(
            get_f64(&ns, "line_overlap"),
            get_f64(&ns, "char_margin"),
            get_f64(&ns, "line_margin"),
            get_f64(&ns, "word_margin"),
            boxes_flow,
            get_bool(&ns, "detect_vertical"),
            get_bool(&ns, "all_texts"),
        )?)
    };

    let mut page_numbers = page_numbers_arg(&ns);
    if let Some(pagenos) = get_str(&ns, "pagenos").filter(|s| !s.is_empty()) {
        page_numbers = Some(pagenos_arg(&pagenos)?);
    }

    let outfile = get_str(&ns, "outfile").unwrap_or_default();
    let mut output_type = get_str(&ns, "output_type").unwrap_or_default();
    if output_type == "text" && outfile != "-" {
        for (override_, alttype) in OUTPUT_TYPES {
            if outfile.ends_with(override_) {
                output_type = alttype.to_string();
            }
        }
    }

    Ok(Args {
        files: get_strs(&ns, "files"),
        outfile,
        output_type: output_type.clone(),
        fp_args: ExtractTextToFp {
            output_type,
            codec: get_str(&ns, "codec").unwrap_or_default(),
            laparams,
            maxpages: get_i64(&ns, "maxpages"),
            page_numbers,
            password: get_str(&ns, "password").unwrap_or_default(),
            scale: get_f64(&ns, "scale"),
            rotation: get_i64(&ns, "rotation"),
            layoutmode: get_str(&ns, "layoutmode").unwrap_or_default(),
            output_dir: get_str(&ns, "output_dir"),
            strip_control: get_bool(&ns, "strip_control"),
            debug: get_bool(&ns, "debug"),
            disable_caching: get_bool(&ns, "disable_caching"),
        },
    })
}

/// `extract_text(files, outfile, ...)`.
fn extract_text(a: &Args, stdout: &Stdout) -> PyResult<()> {
    if a.files.is_empty() {
        return Err(PyErr::new(ExcKind::PDFValueError, "Must provide files to work upon!"));
    }

    let mut fp_args = a.fp_args.clone();
    if a.output_type == "text" && a.outfile != "-" {
        for (override_, alttype) in OUTPUT_TYPES {
            if a.outfile.ends_with(override_) {
                fp_args.output_type = alttype.to_string();
            }
        }
    }

    if a.outfile == "-" {
        let (encoding, errors) = stdout_encoding();
        let outfp: AnyIO = Rc::new(RefCell::new(TextFile::new(stdout.clone(), encoding, errors)));
        let buffer: AnyIO = Rc::new(RefCell::new(BinaryFile::new(stdout.clone())));
        // sys.stdout.encoding is not None
        fp_args.codec = "utf-8".to_string();
        for fname in &a.files {
            let fp = read_file(fname)?;
            extract_text_to_fp(fp, outfp.clone(), Some(buffer.clone()), fp_args.clone())?;
        }
    } else {
        // Use context manager for file output, ensuring proper cleanup
        let file = std::fs::File::create(&a.outfile).map_err(|e| os_error_path(&e, &a.outfile))?;
        let writer = Rc::new(RefCell::new(BinaryFile::new(std::io::BufWriter::new(file))));
        let outfp: AnyIO = writer.clone();
        let r = (|| -> PyResult<()> {
            for fname in &a.files {
                let fp = read_file(fname)?;
                extract_text_to_fp(fp, outfp.clone(), None, fp_args.clone())?;
            }
            Ok(())
        })();
        let f = writer.borrow_mut().inner.flush().map_err(|e| os_error(&e));
        r?;
        f?;
    }
    Ok(())
}

/// `main(args)`: the exit code.
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
    if let Err(e) = finish_args(ns).and_then(|a| extract_text(&a, &stdout)) {
        die(&mut stdout, &e);
    }
    let _ = stdout.flush();
    0
}
