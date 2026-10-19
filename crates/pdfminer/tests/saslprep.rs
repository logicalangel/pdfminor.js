//! `_saslprep` against Python (data from `tools/gen_testdata/saslprep.py`).

use pdfminer::_saslprep::{normalize_nfkc_3_2_0, saslprep};
use sha2::{Digest, Sha256};

fn data() -> serde_json::Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/saslprep.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn nfkc_3_2_0_of_every_character() {
    let d = data();
    let mut h = Sha256::new();
    for cp in 0..0x110000u32 {
        let Some(c) = char::from_u32(cp) else { continue };
        h.update(normalize_nfkc_3_2_0(&c.to_string()).as_bytes());
        h.update(b"\0");
    }
    let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(got, d["nfkc_single_sha256"].as_str().unwrap());
}

#[test]
fn saslprep_cases() {
    let d = data();
    let mut failures = Vec::new();
    for case in d["cases"].as_array().unwrap() {
        let input = case["in"].as_str().unwrap();
        let nfkc = normalize_nfkc_3_2_0(input);
        if nfkc != case["nfkc"].as_str().unwrap() {
            failures.push(format!("nfkc {input:?}: {nfkc:?} != {:?}", case["nfkc"]));
        }
        let got = saslprep(input, true);
        match (&got, case.get("ok")) {
            (Ok(s), Some(want)) if s == want.as_str().unwrap() => {}
            (Err(e), None)
                if e.kind.name() == case["exc"].as_str().unwrap() && e.msg == case["msg"].as_str().unwrap() => {}
            _ => failures.push(format!("saslprep {input:?}: {got:?} != {case}")),
        }
    }
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures[..failures.len().min(30)].join("\n"));
}
