//! Port of `tests/test_font_size.py`.

use pdfminer::high_level::{FileOrName, extract_pages};

#[test]
fn test_font_size() {
    let path = format!("{}/../../upstream/pdfminer.six/samples/font-size-test.pdf", env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0;
    for page in extract_pages(FileOrName::Path(path), "", None, 0, true, None) {
        for text_box in page.unwrap().objs() {
            if !text_box.is_lttextbox() {
                continue;
            }
            for line in text_box.objs() {
                let possible_number = line.get_text().strip();
                let s = possible_number.as_str().unwrap().to_string();
                // str.isdigit()
                if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
                    let expected_size: i64 = s.parse().unwrap();
                    for char in line.objs() {
                        if let Some(c) = char.as_char() {
                            // round(char.size): round half to even
                            assert_eq!(expected_size, c.size.round_ties_even() as i64);
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(checked > 0);
}
