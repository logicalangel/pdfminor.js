//! Port of tests/test_pdfdocument.py.

use std::rc::Rc;

use pdfminer::pdfdocument::PDFDocument;
use pdfminer::pdfpage::PDFPage;
use pdfminer::pdfparser::PDFParser;
use pdfminer::pdftypes::{dict_value, int_value};
use pdfminer::pyrt::error::ExcKind;
use pdfminer::pyrt::io::BytesIO;
use pdfminer::pyrt::value::dict_repr;

fn absolute_sample_path(rel: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../upstream/pdfminer.six/samples").join(rel)
}

fn read(rel: &str) -> Rc<[u8]> {
    Rc::from(std::fs::read(absolute_sample_path(rel)).unwrap())
}

fn open(rel: &str, password: &str) -> Rc<PDFDocument> {
    let parser = PDFParser::new(BytesIO::file(read(rel)));
    PDFDocument::new(parser, password, true, true).unwrap()
}

// --- TestPdfDocument

#[test]
fn test_get_zero_objid_raises_pdfobjectnotfound() {
    let doc = open("simple1.pdf", "");
    assert!(doc.getobj(0).unwrap_err().is(ExcKind::PDFObjectNotFound));
}

#[test]
fn test_encrypted_no_id() {
    // Some documents may be encrypted but not have an /ID key in
    // their trailer. Tests
    // https://github.com/pdfminer/pdfminer.six/issues/594
    let doc = open("encryption/encrypted_doc_no_id.pdf", "");
    let info: Vec<String> = doc.info().iter().map(|d| dict_repr(&d.borrow())).collect();
    assert_eq!(info, vec!["{'Producer': b'European Patent Office'}".to_string()]);
}

#[test]
fn test_page_labels() {
    let doc = open("contrib/pagelabels.pdf", "");
    let pages = dict_value(doc.catalog().borrow().get("Pages").unwrap()).unwrap();
    let total_pages = int_value(pages.borrow().get("Count").unwrap()).unwrap();
    let labels: Vec<String> = doc.get_page_labels().unwrap().take(total_pages as usize).map(|l| l.unwrap()).collect();
    assert_eq!(labels, ["iii", "iv", "1", "2", "1"]);
}

#[test]
fn test_no_page_labels() {
    let doc = open("simple1.pdf", "");
    assert!(doc.get_page_labels().err().unwrap().is(ExcKind::PDFNoPageLabels));
}

#[test]
fn test_annotations() {
    let doc = open("contrib/issue-1082-annotations.pdf", "");
    for page in PDFPage::create_pages(&doc) {
        let page = page.unwrap();
        println!("{}", page.repr());
    }
}
