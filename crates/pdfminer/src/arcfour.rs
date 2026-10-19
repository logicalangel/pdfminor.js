//! Port of `pdfminer/arcfour.py`.
//!
//! Python implementation of Arcfour encryption algorithm.
//! See https://en.wikipedia.org/wiki/RC4
//! This code is in the public domain.

use crate::pyrt::error::{ExcKind, PyErr, PyResult};

/// `Arcfour`.
#[derive(Clone)]
pub struct Arcfour {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Arcfour {
    /// `Arcfour(key)`. An empty key raises `ZeroDivisionError` (`key[i % klen]`), as in Python.
    pub fn new(key: &[u8]) -> PyResult<Self> {
        let mut s = [0u8; 256];
        for (i, v) in s.iter_mut().enumerate() {
            *v = i as u8;
        }
        let mut j: usize = 0;
        let klen = key.len();
        if klen == 0 {
            return Err(PyErr::new(ExcKind::ZeroDivisionError, "division by zero"));
        }
        for i in 0..256 {
            j = (j + s[i] as usize + key[i % klen] as usize) % 256;
            s.swap(i, j);
        }
        Ok(Arcfour { s, i: 0, j: 0 })
    }

    /// `process(data)`.
    pub fn process(&mut self, data: &[u8]) -> Vec<u8> {
        let (mut i, mut j) = (self.i, self.j);
        let s = &mut self.s;
        let mut r = Vec::with_capacity(data.len());
        for &c in data {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[i as usize]);
            s.swap(i as usize, j as usize);
            let k = s[s[i as usize].wrapping_add(s[j as usize]) as usize];
            r.push(c ^ k);
        }
        (self.i, self.j) = (i, j);
        r
    }

    /// `encrypt = process`.
    pub fn encrypt(&mut self, data: &[u8]) -> Vec<u8> {
        self.process(data)
    }

    /// `decrypt = process`.
    pub fn decrypt(&mut self, data: &[u8]) -> Vec<u8> {
        self.process(data)
    }
}

#[cfg(test)]
mod tests {
    // Port of tests/test_pdfminer_crypto.py (TestArcfour).
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|c| format!("{c:02x}")).collect()
    }

    #[test]
    fn test() {
        assert_eq!(hex(&Arcfour::new(b"Key").unwrap().process(b"Plaintext")), "bbf316e8d940af0ad3");
        assert_eq!(hex(&Arcfour::new(b"Wiki").unwrap().process(b"pedia")), "1021bf0420");
        assert_eq!(hex(&Arcfour::new(b"Secret").unwrap().process(b"Attack at dawn")), "45a01f645fc35b383552544b9bf5");
    }

    #[test]
    fn empty_key_raises_zero_division() {
        assert!(Arcfour::new(b"").err().unwrap().is(ExcKind::ZeroDivisionError));
    }

    #[test]
    fn state_carries_over_between_calls() {
        let mut a = Arcfour::new(b"Secret").unwrap();
        let mut out = a.process(b"Attack ");
        out.extend(a.process(b"at dawn"));
        assert_eq!(hex(&out), "45a01f645fc35b383552544b9bf5");
    }
}
