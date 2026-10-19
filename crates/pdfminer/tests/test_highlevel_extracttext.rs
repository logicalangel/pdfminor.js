//! Port of `tests/test_highlevel_extracttext.py`.

use pdfminer::high_level::{FileOrName, extract_pages, extract_text};
use pdfminer::layout::LAParams;
use pdfminer::pyrt::PyStr;

fn absolute_sample_path(path: &str) -> String {
    format!("{}/../../upstream/pdfminer.six/samples/{}", env!("CARGO_MANIFEST_DIR"), path)
}

fn s(p: PyStr) -> String {
    p.as_str().expect("no surrogates").to_string()
}

fn run_with_string(sample_path: &str, laparams: LAParams) -> String {
    let p = FileOrName::Path(absolute_sample_path(sample_path));
    s(extract_text(&p, "", None, 0, true, "utf-8", Some(laparams)).unwrap())
}

fn run_with_file(sample_path: &str) -> String {
    let data = std::fs::read(absolute_sample_path(sample_path)).unwrap();
    s(extract_text(&FileOrName::from(data), "", None, 0, true, "utf-8", None).unwrap())
}

fn strip(t: String) -> String {
    s(PyStr::from(t).strip())
}

fn test_strings(name: &str) -> &'static str {
    match name {
        "simple1.pdf" | "simple1.pdf_no_boxes_flow" => {
            "Hello \n\nWorld\n\nHello \n\nWorld\n\nH e l l o  \n\nW o r l d\n\nH e l l o  \n\nW o r l d\n\n\x0c"
        }
        "simple2.pdf" => "\x0c",
        "simple3.pdf" => "Hello\n\nHello\nあ\nい\nう\nえ\nお\nあ\nい\nう\nえ\nお\nWorld\n\nWorld\n\n\x0c",
        "simple4.pdf" => "Text1\nText2\nText3\n\n\x0c",
        "simple5.pdf" => {
            "Heading\n\nLink to heading that is working with vim-pandoc.\n\nLink to heading “that is” not working with vim-pandoc.\n\nSubheading\n\nSome “more text”\n\n1\n\n\x0c"
        }
        "zen_of_python_corrupted.pdf" => "Mai 30, 18 13:27\n\nzen_of_python.txt",
        "contrib/issue_495_pdfobjref.pdf" => {
            "8\n\n7\n\n6\n\n5\n\n4\n\n3\n\n2\n\n1\n\n150,00\n\n30,00\n\n(cid:72) 0,05 A\n\n0\n0\n,\n0\n2\n\n0\n0\n,\n8\n\n(cid:69) 0,05\n\n0\n0\n,\n0\n5\n\nA\n\nF\n\nE\n\nD\n\n20,00\n\n16,00\n\n+\n0,05\n15,00 - 0,00\n\nC\n\n0\n0\n,\n0\n4\n\n0\n0\n,\n0\n2\n\nR18,00\n\nM12x1.75 - 6H\n\n0\n0\n,\n5\n4\n\nB\n\nA\n\n0\n0\n,\n6\n1\n(cid:142)\n\n0\n0\n,\n6\n1\n\n+\n0,50\n15,00 - 0,00\n\n60,00 (cid:66)0,02\n\n100,00 (cid:66)0,05\n\n132,00\n\n9\nH\n0\n1\n(cid:142)\n\n9\nH\n0\n1\n(cid:142)\n\n(cid:68) 0,1 A\n\n+\n0,00\n70,00 - 0,02\n\n50,00\n\n(cid:76) 0,1\n\n(cid:76) 0,1\n\n0\n0\n,\n5\n3\n\nF\n\nE\n\nD\n\nC\n\nB\n\nAllgemeintoleranzen\n\nMATERIAL\n\nDIN ISO 2768 - mK\n\nPET BLACK\n\nFINISH\n\nEloxieren (natur)\n\nRa 1,6\n\nDate\n29.03.2021\n\nName\nLucas Giering\n\nDrawn\n\nChecked\n\nStandard\n\nArretierungshilfe\n\nA\n\n1 \n\nA2\n\n8\n\n7\n\n6\n\n5\n\n4\n\nState\n\nChanges\n\nDate\n\nName\n\n3\n\n2\n\n1"
        }
        "contrib/issue_566_test_1.pdf" => "ISSUE Date：2019-4-25 Buyer：黎荣",
        "contrib/issue_566_test_2.pdf" => "甲方：中国饮料有限公司（盖章）",
        "contrib/issue-625-identity-cmap.pdf" => "Termin płatności: 2021-05-03",
        "contrib/issue-791-non-unicode-cmap.pdf" => "Peněžní prostředky na účtech",
        "contrib/issue-886-xref-stream-widths.pdf" => "Hello",
        _ => panic!("{name}"),
    }
}

/// `str.splitlines()`.
fn splitlines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' | '\n' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[test]
fn test_simple1_with_string() {
    assert_eq!(run_with_string("simple1.pdf", LAParams::default()), test_strings("simple1.pdf"));
}

#[test]
fn test_simple1_no_boxes_flow() {
    let la = LAParams { boxes_flow: None, ..LAParams::default() };
    assert_eq!(run_with_string("simple1.pdf", la), test_strings("simple1.pdf_no_boxes_flow"));
}

#[test]
fn test_simple2_to_5_with_string() {
    for f in ["simple2.pdf", "simple3.pdf", "simple4.pdf", "simple5.pdf"] {
        assert_eq!(run_with_string(f, LAParams::default()), test_strings(f), "{f}");
    }
}

#[test]
fn test_simple_with_file() {
    for f in ["simple1.pdf", "simple2.pdf", "simple3.pdf", "simple4.pdf", "simple5.pdf"] {
        assert_eq!(run_with_file(f), test_strings(f), "{f}");
    }
}

#[test]
fn test_zlib_corrupted() {
    let text = run_with_file("zen_of_python_corrupted.pdf");
    let expected = test_strings("zen_of_python_corrupted.pdf");
    assert!(text.starts_with(expected), "{text:?}");
}

#[test]
fn test_issue_495_pdfobjref_iterable() {
    let f = "contrib/issue_495_pdfobjref.pdf";
    assert_eq!(strip(run_with_file(f)), test_strings(f));
}

#[test]
fn test_issue_566_cmap_bytes() {
    let f = "contrib/issue_566_test_1.pdf";
    assert_eq!(strip(run_with_file(f)), test_strings(f));
}

#[test]
fn test_issue_566_cid_range() {
    let f = "contrib/issue_566_test_2.pdf";
    assert_eq!(strip(run_with_file(f)), test_strings(f));
}

#[test]
fn test_issue_625_identity_cmap() {
    let f = "contrib/issue-625-identity-cmap.pdf";
    assert_eq!(splitlines(&run_with_file(f))[6], test_strings(f));
}

#[test]
fn test_issue_791_non_unicode_cmap() {
    let f = "contrib/issue-791-non-unicode-cmap.pdf";
    assert_eq!(strip(run_with_file(f)), test_strings(f));
}

/// Ensure that we can support arbitrary width integers in xref streams
#[test]
fn test_issue_886_xref_stream_widths() {
    let f = "contrib/issue-886-xref-stream-widths.pdf";
    assert_eq!(strip(run_with_file(f)), test_strings(f));
}

// TestExtractPages

fn text_containers(laparams: LAParams) -> Vec<String> {
    let path = FileOrName::Path(absolute_sample_path("simple4.pdf"));
    let pages: Vec<_> = extract_pages(path, "", None, 0, true, Some(laparams)).map(|p| p.unwrap()).collect();
    assert_eq!(pages.len(), 1);
    pages[0].objs().iter().filter(|e| e.is_lttextcontainer()).map(|e| s(e.get_text())).collect()
}

#[test]
fn test_line_margin() {
    // The lines have margin 0.2 relative to the height.
    // Extract with line_margin 0.19 should break into 3 separate textboxes.
    let elements = text_containers(LAParams { line_margin: 0.19, ..LAParams::default() });
    assert_eq!(elements, ["Text1\n", "Text2\n", "Text3\n"]);
    // Extract with line_margin 0.21 should merge into one textbox.
    let elements = text_containers(LAParams { line_margin: 0.21, ..LAParams::default() });
    assert_eq!(elements, ["Text1\nText2\nText3\n"]);
}

#[test]
fn test_no_boxes_flow() {
    let elements = text_containers(LAParams { boxes_flow: None, ..LAParams::default() });
    assert_eq!(elements, ["Text1\nText2\nText3\n"]);
}

/// `extract_pages` is lazy: a missing file raises at the first page, like Python's generator.
#[test]
fn test_extract_pages_missing_file_is_lazy() {
    let mut pages = extract_pages(FileOrName::Path("/nonexistent/x.pdf".into()), "", None, 0, true, None);
    let e = pages.next().unwrap().unwrap_err();
    assert_eq!(e.kind.name(), "FileNotFoundError");
    assert_eq!(e.msg, "[Errno 2] No such file or directory: '/nonexistent/x.pdf'");
    assert!(pages.next().is_none());
}
