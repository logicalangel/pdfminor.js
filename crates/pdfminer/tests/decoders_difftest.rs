//! Differential tests of the stream decoders (`ascii85`, `lzw`, `runlength`, `ccitt`, `jbig2`)
//! and of `pyrt::sort::timsort` against pdfminer.six / CPython.
//!
//! The expected results live in `tests/data/<module>/*.json`, written by
//! `tools/gen_testdata/<module>.py` from the pinned Python (random, truncated and corrupted
//! inputs, plus every such stream in the upstream sample PDFs). Every case must give the same
//! bytes, or the same exception class and message.

use std::path::PathBuf;

use pdfminer::pyrt::error::PyResult;
use pdfminer::pyrt::value::{Dict, Value};
use pdfminer::pyrt::zlib;
use serde_json::Value as J;
use sha2::{Digest, Sha256};

fn load(module: &str, name: &str) -> Vec<J> {
    let path: PathBuf =
        [env!("CARGO_MANIFEST_DIR"), "tests", "data", module, &format!("{name}.json")].iter().collect();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let doc: J = serde_json::from_str(&text).unwrap();
    doc["cases"].as_array().unwrap().clone()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn b64decode(s: &str) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = vec![];
    let (mut acc, mut nbits) = (0u32, 0);
    for c in s.bytes().filter(|&c| c != b'=') {
        acc = (acc << 6) | T.iter().position(|&t| t == c).unwrap() as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    out
}

fn input(case: &J) -> Vec<u8> {
    if let Some(h) = case.get("in") {
        unhex(h.as_str().unwrap())
    } else {
        zlib::decompress(&b64decode(case["in_z"].as_str().unwrap())).unwrap()
    }
}

/// Rebuild a Python object encoded by `common.encode_value`.
fn value(j: &J) -> Value {
    match j {
        J::Null => Value::None,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => Value::Int(n.as_i64().unwrap()),
        J::Object(o) => {
            let v = &o["v"];
            match o["t"].as_str().unwrap() {
                "float" => Value::Float(v.as_str().unwrap().parse().unwrap()),
                "bytes" => Value::bytes(&unhex(v.as_str().unwrap())),
                "str" => Value::str(v.as_str().unwrap()),
                "lit" => Value::Literal(pdfminer::psparser::LIT(v.as_str().unwrap())),
                "list" => Value::list(v.as_array().unwrap().iter().map(value).collect()),
                "dict" => {
                    let mut d = Dict::new();
                    for kv in v.as_array().unwrap() {
                        d.insert(kv[0].as_str().unwrap().into(), value(&kv[1]));
                    }
                    Value::dict(d)
                }
                t => panic!("unknown tag {t}"),
            }
        }
        other => panic!("unexpected {other}"),
    }
}

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// Compare written bytes with the case's `out` / `out_len`+`out_sha256`.
fn check_bytes(case: &J, got: &[u8]) -> Result<(), String> {
    if let Some(out) = case.get("out") {
        let want = unhex(out.as_str().unwrap());
        if got != want {
            return Err(format!("output differs: got {} bytes, want {} bytes", got.len(), want.len()));
        }
    } else {
        let want_len = case["out_len"].as_u64().unwrap() as usize;
        if got.len() != want_len || sha256_hex(got) != case["out_sha256"].as_str().unwrap() {
            return Err(format!("output differs: got {} bytes, want {want_len} bytes", got.len()));
        }
    }
    Ok(())
}

fn check_err(case: &J, e: &pdfminer::pyrt::error::PyErr) -> Result<(), String> {
    let (want, msg) = (case["err"].as_str().unwrap(), case["msg"].as_str().unwrap());
    if e.kind.name() != want || e.msg != msg {
        return Err(format!("raised {:?}({:?}), want {want}({msg:?})", e.kind, e.msg));
    }
    Ok(())
}

fn check(case: &J, got: PyResult<Vec<u8>>) -> Result<(), String> {
    match (case.get("err"), got) {
        (None, Ok(out)) => check_bytes(case, &out),
        (Some(_), Err(e)) => check_err(case, &e),
        (None, Err(e)) => Err(format!("raised {:?}({:?}), want a result", e.kind, e.msg)),
        (Some(_), Ok(_)) => Err(format!("returned, want {}", case["err"])),
    }
}

fn run(module: &str, name: &str, f: impl Fn(&J, &[u8]) -> Result<(), String>) {
    let cases = load(module, name);
    let mut failures = vec![];
    for (i, case) in cases.iter().enumerate() {
        let data = input(case);
        if let Err(msg) = f(case, &data) {
            failures.push(format!("case {i} (input {} bytes): {msg}", data.len()));
        }
    }
    assert!(failures.is_empty(), "{}/{} {name} cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
    assert!(cases.len() > 100);
}

#[test]
fn ascii85decode_matches_python() {
    run("ascii85", "ascii85decode", |c, d| check(c, pdfminer::ascii85::ascii85decode(d)));
}

#[test]
fn asciihexdecode_matches_python() {
    run("ascii85", "asciihexdecode", |c, d| check(c, pdfminer::ascii85::asciihexdecode(d)));
}

#[test]
fn lzwdecode_matches_python() {
    run("lzw", "lzwdecode", |c, d| check(c, pdfminer::lzw::lzwdecode(d)));
}

#[test]
fn rldecode_matches_python() {
    run("runlength", "rldecode", |c, d| check(c, pdfminer::runlength::rldecode(d)));
}

#[test]
fn ccittfaxdecode_matches_python() {
    run("ccitt", "ccittfaxdecode", |c, d| check(c, pdfminer::ccitt::ccittfaxdecode(d, &value(&c["params"]))));
}

#[test]
fn jbig2_matches_python() {
    run("jbig2", "convert", |c, d| {
        let (out, err) = pdfminer::jbig2::jbig2_convert(d, c["fix_last_page"].as_bool().unwrap());
        check_bytes(c, &out)?;
        match (c.get("err"), err) {
            (None, None) => Ok(()),
            (Some(_), Some(e)) => check_err(c, &e),
            (None, Some(e)) => Err(format!("raised {:?}({:?}), want no exception", e.kind, e.msg)),
            (Some(w), None) => Err(format!("no exception, want {w}")),
        }
    });
}

// timsort: keys built from a spec, as in tools/gen_testdata/timsort.py.

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
}

fn make_keys(seed: u64, n: u64, pattern: u64, distinct: u64, nan_pm: u64, noise_pm: u64) -> Vec<f64> {
    let mut g = SplitMix64(seed);
    let period = distinct + 1;
    (0..n)
        .map(|i| {
            let r = g.next();
            let mut v = match pattern {
                0 => r % distinct,
                1 => i * distinct / n,
                2 => (n - i) * distinct / n,
                3 => i % period,
                _ => period - i % period,
            };
            if pattern != 0 && g.next() % 1000 < noise_pm {
                v = r % distinct;
            }
            if g.next() % 1000 < nan_pm { f64::NAN } else { v as f64 }
        })
        .collect()
}

fn fnv1a(perm: &[usize]) -> u64 {
    let mut h: u64 = 0xCBF29CE484222325;
    for &i in perm {
        for b in (i as u32).to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001B3);
        }
    }
    h
}

#[test]
fn listsort_matches_cpython() {
    let cases = load("timsort", "listsort");
    let mut failures = vec![];
    for (i, c) in cases.iter().enumerate() {
        let u = |k: &str| c[k].as_u64().unwrap();
        let seed: u64 = c["seed"].as_str().unwrap().parse().unwrap();
        let keys = make_keys(seed, u("n"), u("pattern"), u("distinct"), u("nan_pm"), u("noise_pm"));
        let mut perm: Vec<usize> = (0..keys.len()).collect();
        pdfminer::pyrt::sort::timsort(&mut perm, &|&a: &usize, &b: &usize| keys[a] < keys[b]);
        let want_fnv: u64 = c["fnv"].as_str().unwrap().parse().unwrap();
        if let Some(want) = c.get("perm") {
            let want: Vec<usize> = want.as_array().unwrap().iter().map(|x| x.as_u64().unwrap() as usize).collect();
            if perm != want {
                failures.push(format!("case {i} (n={}): {perm:?} != {want:?}", keys.len()));
                continue;
            }
        }
        if fnv1a(&perm) != want_fnv {
            failures.push(format!("case {i} (n={}): permutation hash differs", keys.len()));
        }
    }
    assert!(failures.is_empty(), "{}/{} sorts differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}
