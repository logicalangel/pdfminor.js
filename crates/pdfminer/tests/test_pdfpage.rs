//! Port of tests/test_pdfpage.py, plus checks of `PDFPage.get_pages`.

use std::collections::HashSet;
use std::rc::Rc;

use pdfminer::pdfdocument::PDFDocument;
use pdfminer::pdfpage::PDFPage;
use pdfminer::pdfparser::PDFParser;
use pdfminer::pyrt::error::ExcKind;
use pdfminer::pyrt::io::BytesIO;
use pdfminer::pyrt::value::Value;

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

// --- TestPdfPage

#[test]
fn test_page_labels() {
    let expected_labels = ["iii", "iv", "1", "2", "1"];
    let doc = open("contrib/pagelabels.pdf", "");
    for (i, page) in PDFPage::create_pages(&doc).enumerate() {
        assert_eq!(page.unwrap().label.as_deref(), Some(expected_labels[i]));
    }
}

// --- get_pages

#[test]
fn get_pages_is_lazy_and_honours_pagenos_and_maxpages() {
    let all: Vec<Value> = PDFPage::get_pages(read("contrib/pagelabels.pdf"), None, 0, "", true, false)
        .map(|p| p.unwrap().pageid)
        .collect();
    assert_eq!(all.len(), 5);
    let some: Vec<String> =
        PDFPage::get_pages(read("contrib/pagelabels.pdf"), Some(HashSet::from([1, 3, 9])), 0, "", true, false)
            .map(|p| p.unwrap().pageid.repr())
            .collect();
    assert_eq!(some, vec![all[1].repr(), all[3].repr()]);
    let first2 = PDFPage::get_pages(read("contrib/pagelabels.pdf"), None, 2, "", true, false).count();
    assert_eq!(first2, 2);
    // maxpages only counts pages that were yielded.
    let n = PDFPage::get_pages(read("contrib/pagelabels.pdf"), Some(HashSet::from([3, 4])), 4, "", true, false).count();
    assert_eq!(n, 1);
}

#[test]
fn get_pages_reports_errors_on_first_next() {
    let mut it = PDFPage::get_pages(Rc::from(&b"not a pdf"[..]), None, 0, "", true, false);
    assert!(it.next().unwrap().is_err());
    assert!(it.next().is_none());
}

#[test]
fn get_pages_encrypted() {
    for (rel, pw) in [
        ("encryption/aes-128.pdf", "foo"),
        ("encryption/aes-128-m.pdf", "foo"),
        ("encryption/aes-256.pdf", "foo"),
        ("encryption/aes-256-m.pdf", "foo"),
        ("encryption/aes-256-r6.pdf", "usersecret"),
        ("encryption/aes-256-r6.pdf", "ownersecret"),
        ("encryption/base.pdf", "foo"),
        ("encryption/rc4-40.pdf", "foo"),
        ("encryption/rc4-128.pdf", "foo"),
    ] {
        let pages: Vec<_> = PDFPage::get_pages(read(rel), None, 0, pw, true, false).collect();
        assert!(!pages.is_empty(), "{rel}");
        for p in pages {
            let p = p.unwrap();
            for c in &p.contents {
                let s = pdfminer::pdftypes::stream_value(c).unwrap();
                assert!(!s.get_data().unwrap().is_empty(), "{rel}");
            }
        }
    }
    let mut it = PDFPage::get_pages(read("encryption/aes-256-r6.pdf"), None, 0, "wrong", true, false);
    assert!(it.next().unwrap().err().unwrap().is(ExcKind::PDFPasswordIncorrect));
}
