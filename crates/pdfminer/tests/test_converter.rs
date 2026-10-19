//! Port of `tests/test_converter.py`.
//!
//! `TestColorSpace.test_pattern_operators` drives the interpreter's operators directly; it is in
//! the tests of `pdfinterp.rs`. `TestBinaryDetector.test_non_file_like_object_defaults_to_binary`
//! and `test_textiowrapper` have no counterpart: every stream the port writes to declares whether
//! it is binary (`PyFile::is_binary`).

use std::cell::RefCell;
use std::rc::Rc;

use pdfminer::converter::{PDFConverter, PDFLayoutAnalyzer};
use pdfminer::high_level::{FileOrName, extract_pages};
use pdfminer::layout::{LTClass, LTContainer, LTItem};
use pdfminer::pdfinterp::{Color, PDFGraphicState, PDFGraphicStateRef, PDFResourceManager, StandardColor};
use pdfminer::pyrt::textio::{AnyIO, BinaryFile, BytesOut, Codec, Errors, StringIO, TextFile};
use pdfminer::pyrt::value::Value;
use pdfminer::utils::PathSegment;

fn sample(path: &str) -> String {
    format!("{}/../../upstream/pdfminer.six/samples/{}", env!("CARGO_MANIFEST_DIR"), path)
}

fn seg(op: char, args: &[f64]) -> PathSegment {
    PathSegment { op, args: args.to_vec() }
}

fn gs() -> PDFGraphicStateRef {
    Rc::new(RefCell::new(PDFGraphicState::new()))
}

fn _get_analyzer() -> PDFLayoutAnalyzer {
    let mut analyzer = PDFLayoutAnalyzer::new(PDFResourceManager::new(true), 1, None);
    analyzer.ctm = Some((1.0, 0.0, 0.0, 1.0, 0.0, 0.0));
    analyzer
}

fn parse_with(path: &[PathSegment], graphicstate: PDFGraphicStateRef) -> Vec<LTItem> {
    let mut analyzer = _get_analyzer();
    analyzer.cur_item = Some(LTContainer((0.0, 1000.0, 0.0, 1000.0)));
    analyzer.paint_path(&graphicstate, false, false, false, path).unwrap();
    analyzer.cur_item.unwrap().objs()
}

fn parse(path: &[PathSegment]) -> Vec<LTItem> {
    parse_with(path, gs())
}

fn get_types(path: &[PathSegment]) -> Vec<LTClass> {
    parse(path).iter().map(|o| o.class()).collect()
}

fn painted(paths: &[Vec<PathSegment>]) -> usize {
    let mut analyzer = _get_analyzer();
    analyzer.cur_item = Some(LTContainer((0.0, 100.0, 0.0, 100.0)));
    for path in paths {
        analyzer.paint_path(&gs(), false, false, false, path).unwrap();
    }
    analyzer.cur_item.unwrap().len()
}

#[test]
fn test_paint_path() {
    assert_eq!(painted(&[vec![seg('m', &[6.0, 7.0]), seg('l', &[7.0, 7.0])]]), 1);
}

#[test]
fn test_paint_path_mlllh() {
    let path = vec![
        seg('m', &[6.0, 7.0]),
        seg('l', &[7.0, 7.0]),
        seg('l', &[7.0, 91.0]),
        seg('l', &[6.0, 91.0]),
        seg('h', &[]),
    ];
    assert_eq!(painted(&[path]), 1);
}

/// Path from samples/contrib/issue-00369-excel.pdf
#[test]
fn test_paint_path_multiple_mlllh() {
    let path = vec![
        seg('m', &[6.0, 7.0]),
        seg('l', &[7.0, 7.0]),
        seg('l', &[7.0, 91.0]),
        seg('l', &[6.0, 91.0]),
        seg('h', &[]),
        seg('m', &[4.0, 7.0]),
        seg('l', &[6.0, 7.0]),
        seg('l', &[6.0, 91.0]),
        seg('l', &[4.0, 91.0]),
        seg('h', &[]),
        seg('m', &[67.0, 2.0]),
        seg('l', &[68.0, 2.0]),
        seg('l', &[68.0, 3.0]),
        seg('l', &[67.0, 3.0]),
        seg('h', &[]),
    ];
    assert_eq!(painted(&[path]), 3);
}

fn poly(pts: &[(f64, f64)], close: bool) -> Vec<PathSegment> {
    let mut p = vec![seg('m', &[pts[0].0, pts[0].1])];
    for q in &pts[1..] {
        p.push(seg('l', &[q.0, q.1]));
    }
    if close {
        p.push(seg('h', &[]));
    }
    p
}

/// Via https://github.com/pdfminer/pdfminer.six/issues/473
#[test]
fn test_paint_path_quadrilaterals() {
    use LTClass::*;
    // Standard rect
    assert_eq!(get_types(&poly(&[(10., 90.), (90., 90.), (90., 10.), (10., 10.)], true)), [LTRect]);
    // Same but mllll variation
    assert_eq!(get_types(&poly(&[(10., 90.), (90., 90.), (90., 10.), (10., 10.), (10., 90.)], false)), [LTRect]);
    // Same but mllllh variation
    assert_eq!(get_types(&poly(&[(10., 90.), (90., 90.), (90., 10.), (10., 10.), (10., 90.)], true)), [LTRect]);
    // Bowtie shape
    assert_eq!(get_types(&poly(&[(110., 90.), (190., 10.), (190., 90.), (110., 10.)], true)), [LTCurve]);
    // Quadrilateral with one slanted side
    assert_eq!(get_types(&poly(&[(210., 90.), (290., 60.), (290., 10.), (210., 10.)], true)), [LTCurve]);
    // Path with two rect subpaths
    let mut p = poly(&[(310., 90.), (350., 90.), (350., 10.), (310., 10.)], true);
    p.extend(poly(&[(350., 90.), (390., 90.), (390., 10.), (350., 10.)], true));
    assert_eq!(get_types(&p), [LTRect, LTRect]);
    // Path with one rect subpath and one pentagon
    let mut p = poly(&[(410., 90.), (445., 90.), (445., 10.), (410., 10.)], true);
    p.extend(poly(&[(455., 70.), (475., 90.), (490., 70.), (490., 10.), (455., 10.)], true));
    assert_eq!(get_types(&p), [LTRect, LTCurve]);
    // Three types of simple lines
    let mut p = poly(&[(10., 30.), (10., 40.)], true);
    p.extend(poly(&[(10., 50.), (70., 50.)], true));
    p.extend(poly(&[(10., 10.), (30., 30.)], true));
    assert_eq!(get_types(&p), [LTLine, LTLine, LTLine]);
    // Same as above, but 'ml' variation
    let mut p = poly(&[(10., 30.), (10., 40.)], false);
    p.extend(poly(&[(10., 50.), (70., 50.)], false));
    p.extend(poly(&[(10., 10.), (30., 30.)], false));
    assert_eq!(get_types(&p), [LTLine, LTLine, LTLine]);

    // There are six lines in this one-page PDF; they all have shape 'ml' not 'mlh'
    let mut ml_pdf = extract_pages(FileOrName::Path(sample("contrib/pr-00530-ml-lines.pdf")), "", None, 0, true, None);
    let ml_pdf_page = ml_pdf.next().unwrap().unwrap();
    assert_eq!(ml_pdf_page.objs().iter().filter(|o| o.class() == LTLine).count(), 6);
}

/// See section 4.4, table 4.9 of the PDF reference manual
#[test]
fn test_paint_path_beziers() {
    // "c" operator
    let r = parse(&[seg('m', &[72.41, 433.89]), seg('c', &[72.41, 434.45, 71.96, 434.89, 71.41, 434.89])]);
    assert_eq!(r[0].as_curve().unwrap().pts, [(72.41, 433.89), (71.41, 434.89)]);
    // "v" operator
    let r = parse(&[seg('m', &[72.41, 433.89]), seg('v', &[71.96, 434.89, 71.41, 434.89])]);
    assert_eq!(r[0].as_curve().unwrap().pts, [(72.41, 433.89), (71.41, 434.89)]);
    // "y" operator
    let r = parse(&[seg('m', &[72.41, 433.89]), seg('y', &[72.41, 434.45, 71.41, 434.89])]);
    assert_eq!(r[0].as_curve().unwrap().pts, [(72.41, 433.89), (71.41, 434.89)]);
}

#[test]
fn test_paint_path_beziers_check_raw() {
    let r = parse(&[seg('m', &[72.41, 433.89]), seg('c', &[72.41, 434.45, 71.96, 434.89, 71.41, 434.89])]);
    assert_eq!(
        r[0].as_curve().unwrap().original_path,
        Some(vec![
            ("m".to_string(), vec![(72.41, 433.89)]),
            ("c".to_string(), vec![(72.41, 434.45), (71.96, 434.89), (71.41, 434.89)]),
        ])
    );
}

#[test]
fn test_paint_path_dashed() {
    let graphicstate = gs();
    graphicstate.borrow_mut().dash = Some((Value::list(vec![Value::Int(1), Value::Int(1)]), Value::Int(0)));
    let r = parse_with(
        &[seg('m', &[72.41, 433.89]), seg('c', &[72.41, 434.45, 71.96, 434.89, 71.41, 434.89])],
        graphicstate,
    );
    let (dash, phase) = r[0].as_curve().unwrap().dashing_style.clone().unwrap();
    assert_eq!(dash.repr(), "[1, 1]");
    assert_eq!(phase.repr(), "0");
}

#[test]
fn test_paint_path_without_starting_m() {
    let paths = [vec![seg('h', &[])], vec![seg('l', &[72.41, 433.89]), seg('l', &[82.41, 433.89]), seg('h', &[])]];
    assert_eq!(painted(&paths), 0);
}

#[test]
fn test_linewidth() {
    let mut ml_pdf =
        extract_pages(FileOrName::Path(sample("contrib/issue_1165_linewidth.pdf")), "", None, 0, true, None);
    let ml_pdf_page = ml_pdf.next().unwrap().unwrap();
    let mut lines: Vec<f64> = ml_pdf_page
        .objs()
        .iter()
        .filter(|o| o.class() == LTClass::LTLine)
        .map(|o| o.as_curve().unwrap().linewidth.as_f64().unwrap())
        .collect();
    lines.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], 2.83465);
    assert_eq!(lines[1], 2.0 * 2.83465);
}

fn get_chars(el: &LTItem, out: &mut Vec<LTItem>) {
    if el.is_ltcontainer() {
        for item in el.objs() {
            get_chars(&item, out);
        }
    } else if el.is_ltchar() {
        out.push(el.clone());
    }
}

#[test]
fn test_do_rg() {
    let path = sample("contrib/issue-00352-hash-twos-complement.pdf");
    for page in extract_pages(FileOrName::Path(path), "", None, 0, true, None) {
        let mut chars = Vec::new();
        get_chars(&page.unwrap(), &mut chars);
        for char in chars {
            let c = char.as_char().unwrap();
            let color = c.graphicstate.borrow().ncolor.clone();
            match c.ncs.name() {
                "DeviceGray" => {
                    assert!(matches!(color, Color::Standard(StandardColor::Gray(_) | StandardColor::IntGray(_))))
                }
                "DeviceRGB" => assert!(matches!(color, Color::Standard(StandardColor::Rgb(..)))),
                "DeviceCMYK" => assert!(matches!(color, Color::Standard(StandardColor::Cmyk(..)))),
                // Pattern colors should be stored as strings (pattern names)
                "Pattern" => {
                    assert!(matches!(&color, Color::Pattern(name) if name.starts_with('P')))
                }
                _ => {}
            }
        }
    }
}

/// Test that Pattern color spaces are properly handled.
#[test]
fn test_pattern_colors() {
    let path = sample("test_pattern_colors.pdf");
    for page in extract_pages(FileOrName::Path(path), "", None, 0, true, None) {
        for item in page.unwrap().objs() {
            if let Some(c) = item.as_curve() {
                for color in [&c.stroking_color, &c.non_stroking_color].into_iter().flatten() {
                    match color {
                        // Colored pattern: stored as string
                        Color::Pattern(name) => assert!(name.starts_with('P')),
                        // Uncolored pattern: stored as tuple (base_color, pattern_name)
                        Color::UncoloredPattern(_, name) => assert!(name.starts_with('P')),
                        Color::Standard(_) => {}
                    }
                }
            }
        }
    }
}

// TestBinaryDetector

#[test]
fn test_stringio() {
    let f: AnyIO = Rc::new(RefCell::new(StringIO::new()));
    assert!(!PDFConverter::_is_binary_stream(&f));
}

#[test]
fn test_bytesio() {
    let f: AnyIO = Rc::new(RefCell::new(BytesOut::new()));
    assert!(PDFConverter::_is_binary_stream(&f));
}

#[test]
fn test_tmpfile() {
    let f: AnyIO = Rc::new(RefCell::new(TextFile::new(Vec::<u8>::new(), Codec::Utf8, Errors::Strict)));
    assert!(!PDFConverter::_is_binary_stream(&f));
}

#[test]
fn test_binary_tmpfile() {
    let f: AnyIO = Rc::new(RefCell::new(BinaryFile::new(Vec::<u8>::new())));
    assert!(PDFConverter::_is_binary_stream(&f));
}
