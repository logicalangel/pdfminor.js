//! CPython's `zlib.decompress` and `zlib.decompressobj().decompress`, on the real zlib (through
//! flate2's `zlib` backend), so corrupted streams fail at the same byte as in Python.

use flate2::{Decompress, FlushDecompress, Status};

use super::error::{ExcKind, PyErr, PyResult};

fn zlib_error(msg: &str) -> PyErr {
    PyErr::new(ExcKind::ZlibError, format!("Error while decompressing data: {msg}"))
}

/// `zlib.decompress(data)`: CPython feeds all input with `Z_FINISH`, inflates while the output
/// buffer fills up, and fails unless the stream ended (`incomplete or truncated stream`). Bytes
/// after the end of the stream are ignored.
pub fn decompress(data: &[u8]) -> PyResult<Vec<u8>> {
    let mut d = Decompress::new(true);
    let mut out: Vec<u8> = Vec::with_capacity(data.len().saturating_mul(4).max(64));
    loop {
        if out.len() == out.capacity() {
            out.reserve(out.capacity().max(64));
        }
        let consumed = d.total_in() as usize;
        let status = d
            .decompress_vec(&data[consumed..], &mut out, FlushDecompress::Finish)
            .map_err(|e| zlib_error(&e.to_string()))?;
        if status == Status::StreamEnd {
            return Ok(out);
        }
        // `while (zst.avail_out == 0)`: only a full output buffer means "call inflate again".
        if out.len() < out.capacity() {
            return Err(zlib_error("incomplete or truncated stream"));
        }
    }
}

/// `zlib.decompressobj()`.
pub struct DecompressObj {
    d: Decompress,
    eof: bool,
}

impl Default for DecompressObj {
    fn default() -> Self {
        Self::new()
    }
}

impl DecompressObj {
    pub fn new() -> Self {
        DecompressObj { d: Decompress::new(true), eof: false }
    }

    /// `d.decompress(data)`: everything decodable from the input so far. On an error, the output
    /// of this call is lost (Python raises before returning it). After the end of the stream,
    /// further input is kept aside (`unused_data`) without error.
    pub fn decompress(&mut self, data: &[u8]) -> PyResult<Vec<u8>> {
        let mut out: Vec<u8> = Vec::with_capacity(64);
        if self.eof {
            return Ok(out);
        }
        let start_in = self.d.total_in();
        loop {
            if out.len() == out.capacity() {
                out.reserve(out.capacity().max(64));
            }
            let consumed = (self.d.total_in() - start_in) as usize;
            let status = self
                .d
                .decompress_vec(&data[consumed..], &mut out, FlushDecompress::Sync)
                .map_err(|e| zlib_error(&e.to_string()))?;
            if status == Status::StreamEnd {
                self.eof = true;
                return Ok(out);
            }
            // `while (self->zst.avail_out == 0)`
            if out.len() < out.capacity() {
                return Ok(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &[u8] = &[120, 156, 203, 72, 205, 201, 201, 87, 40, 207, 47, 202, 73, 1, 0, 26, 11, 4, 93];

    #[test]
    fn decompress_whole_and_truncated() {
        assert_eq!(decompress(HELLO).unwrap(), b"hello world");
        assert!(decompress(&HELLO[..10]).unwrap_err().is(ExcKind::ZlibError));
        let mut with_garbage = HELLO.to_vec();
        with_garbage.extend_from_slice(b"garbage");
        assert_eq!(decompress(&with_garbage).unwrap(), b"hello world");
        let mut bad_check = HELLO.to_vec();
        *bad_check.last_mut().unwrap() ^= 1;
        assert!(decompress(&bad_check).is_err());
    }

    #[test]
    fn decompressobj_byte_by_byte() {
        let mut d = DecompressObj::new();
        let mut out = vec![];
        for b in HELLO {
            out.extend(d.decompress(&[*b]).unwrap());
        }
        assert_eq!(out, b"hello world");
    }
}
