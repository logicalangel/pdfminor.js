//! Port of `pdfminer/lzw.py`.

use std::rc::Rc;

use log::debug;

use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::bytes_repr;
use crate::pyrt::io::BytesIO;

/// `class CorruptDataError(PDFException)` is [`ExcKind::CorruptDataError`].
pub struct LZWDecoder {
    pub fp: BytesIO,
    pub buff: u32,
    pub bpos: u32,
    pub nbits: u32,
    /// NB: self.table stores None only in indices 256 and 257
    pub table: Vec<Option<Rc<[u8]>>>,
    pub prevbuf: Option<Rc<[u8]>>,
}

impl LZWDecoder {
    pub fn new(fp: BytesIO) -> Self {
        LZWDecoder { fp, buff: 0, bpos: 8, nbits: 9, table: vec![], prevbuf: None }
    }

    /// `readbits(bits)`: raises `PDFEOFError` at the end of the input.
    pub fn readbits(&mut self, mut bits: u32) -> PyResult<u32> {
        let mut v: u32 = 0;
        loop {
            // the number of remaining bits we can get from the current buffer.
            let r = 8 - self.bpos;
            if bits <= r {
                // |-----8-bits-----|
                // |-bpos-|-bits-|  |
                // |      |----r----|
                v = (v << bits) | ((self.buff >> (r - bits)) & ((1 << bits) - 1));
                self.bpos += bits;
                break;
            } else {
                // |-----8-bits-----|
                // |-bpos-|---bits----...
                // |      |----r----|
                v = (v << r) | (self.buff & ((1 << r) - 1));
                bits -= r;
                let x = self.fp.read(1);
                if x.is_empty() {
                    return Err(PyErr::new(ExcKind::PDFEOFError, ""));
                }
                self.buff = x[0] as u32;
                self.bpos = 0;
            }
        }
        Ok(v)
    }

    /// `feed(code)`: raises `CorruptDataError` on a code beyond the table, and `IndexError`
    /// (from `self.table[code]`) on a code before the table was initialized.
    pub fn feed(&mut self, code: u32) -> PyResult<Rc<[u8]>> {
        let mut x: Rc<[u8]> = Rc::from(&b""[..]);
        let code_u = code as usize;
        if code == 256 {
            self.table = (0..=255u8).map(|c| Some(Rc::from(&[c][..]))).collect(); // 0-255
            self.table.push(None); // 256
            self.table.push(None); // 257
            self.prevbuf = Some(Rc::from(&b""[..]));
            self.nbits = 9;
        } else if code == 257 {
        } else if self.prevbuf.as_ref().is_none_or(|p| p.is_empty()) {
            // x = self.prevbuf = cast(bytes, self.table[code])  # assume not None
            let entry = self.table.get(code_u).ok_or_else(list_index_error)?;
            // Only 256 and 257 are None, and they are handled above.
            x = entry.clone().expect("table entry");
            self.prevbuf = Some(x.clone());
        } else {
            let prevbuf = self.prevbuf.clone().expect("prevbuf");
            if code_u < self.table.len() {
                x = self.table[code_u].clone().expect("table entry"); // assume not None
                let mut e = Vec::with_capacity(prevbuf.len() + 1);
                e.extend_from_slice(&prevbuf);
                e.extend_from_slice(&x[..1]);
                self.table.push(Some(Rc::from(e)));
            } else if code_u == self.table.len() {
                let mut e = Vec::with_capacity(prevbuf.len() + 1);
                e.extend_from_slice(&prevbuf);
                e.extend_from_slice(&prevbuf[..1]);
                self.table.push(Some(Rc::from(e)));
                x = self.table[code_u].clone().expect("table entry");
            } else {
                return Err(PyErr::new(ExcKind::CorruptDataError, ""));
            }
            let table_length = self.table.len();
            if table_length == 511 {
                self.nbits = 10;
            } else if table_length == 1023 {
                self.nbits = 11;
            } else if table_length == 2047 {
                self.nbits = 12;
            }
            self.prevbuf = Some(x.clone());
        }
        Ok(x)
    }

    /// `run()`: the generator, as a callback per yielded chunk. An exception other than the
    /// ones `run` catches propagates after the chunks yielded so far.
    pub fn run(&mut self, mut emit: impl FnMut(&[u8])) -> PyResult<()> {
        loop {
            let code = match self.readbits(self.nbits) {
                Ok(code) => code,
                Err(e) if e.is(ExcKind::EOFError) => break,
                Err(e) => return Err(e),
            };
            let x = match self.feed(code) {
                Ok(x) => x,
                // just ignore corrupt data and stop yielding there
                Err(e) if e.is(ExcKind::CorruptDataError) => break,
                Err(e) => return Err(e),
            };
            emit(&x);

            if log::log_enabled!(log::Level::Debug) {
                debug!(
                    "nbits={}, code={}, output={}, table=[{}]",
                    self.nbits,
                    code,
                    bytes_repr(&x),
                    self.table
                        .iter()
                        .skip(258)
                        .map(|e| e.as_ref().map_or("None".to_string(), |b| bytes_repr(b)))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        Ok(())
    }
}

fn list_index_error() -> PyErr {
    PyErr::new(ExcKind::IndexError, "list index out of range")
}

/// `lzwdecode(data)`.
pub fn lzwdecode(data: &[u8]) -> PyResult<Vec<u8>> {
    let fp = BytesIO::new(Rc::from(data));
    let mut out = Vec::new();
    LZWDecoder::new(fp).run(|x| out.extend_from_slice(x))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // tests/test_pdfminer_crypto.py: TestLzw

    #[test]
    fn test_lzwdecode() {
        assert_eq!(
            lzwdecode(b"\x80\x0b\x60\x50\x22\x0c\x0c\x85\x01").unwrap(),
            b"\x2d\x2d\x2d\x2d\x2d\x41\x2d\x2d\x2d\x42"
        );
    }

    #[test]
    fn no_clear_code_is_index_error() {
        // The first code is 0 and the table is empty: `self.table[code]` raises IndexError.
        assert!(lzwdecode(b"\x00\x00").unwrap_err().is(ExcKind::IndexError));
        assert_eq!(lzwdecode(b"").unwrap(), b"");
    }
}
