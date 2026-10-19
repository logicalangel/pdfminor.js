//! `io.BytesIO` (and a binary file opened with `open(path, "rb")`, which behaves the same for
//! pdfminer: it only seeks, tells and reads).

use std::rc::Rc;

use super::error::{ExcKind, PyErr, PyResult};

/// A seekable byte stream over shared, immutable data.
#[derive(Clone)]
pub struct BytesIO {
    data: Rc<[u8]>,
    pos: usize,
    /// What a negative seek raises: `ValueError` for `io.BytesIO`, `OSError` for a real file.
    negative_seek: ExcKind,
}

impl BytesIO {
    /// An `io.BytesIO(data)`.
    pub fn new(data: Rc<[u8]>) -> Self {
        BytesIO { data, pos: 0, negative_seek: ExcKind::ValueError }
    }

    pub fn from_vec(data: Vec<u8>) -> Self {
        BytesIO::new(Rc::from(data))
    }

    /// A file opened with `open(path, "rb")`, read into memory.
    pub fn file(data: Rc<[u8]>) -> Self {
        BytesIO { data, pos: 0, negative_seek: ExcKind::OSError }
    }

    /// `fp.seek(pos)`. Seeking past the end is allowed (reads then return `b""`); a negative
    /// position raises.
    pub fn seek(&mut self, pos: i64) -> PyResult<()> {
        if pos < 0 {
            return Err(match self.negative_seek {
                ExcKind::OSError => PyErr::new(ExcKind::OSError, "[Errno 22] Invalid argument"),
                k => PyErr::new(k, format!("negative seek value {pos}")),
            });
        }
        self.pos = pos as usize;
        Ok(())
    }

    /// `fp.seek(0, io.SEEK_END)`.
    pub fn seek_end(&mut self) {
        self.pos = self.data.len();
    }

    /// `fp.tell()`.
    pub fn tell(&self) -> i64 {
        self.pos as i64
    }

    /// `fp.read(n)`; a negative `n` reads to the end, as in Python.
    pub fn read(&mut self, n: i64) -> &[u8] {
        if n < 0 {
            return self.read_all();
        }
        let start = self.pos.min(self.data.len());
        let end = start.saturating_add(n as usize).min(self.data.len());
        self.pos = self.pos.max(end);
        &self.data[start..end]
    }

    /// `fp.read()` (to the end).
    pub fn read_all(&mut self) -> &[u8] {
        let start = self.pos.min(self.data.len());
        self.pos = self.data.len().max(self.pos);
        &self.data[start..]
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn data(&self) -> &Rc<[u8]> {
        &self.data
    }
}
