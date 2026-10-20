//! Port of `pdfminer/pdfparser.py`.

use std::rc::{Rc, Weak};

use log::debug;

use crate::casting::safe_int_obj;
use crate::pdfdocument::PDFDocument;
use crate::pdftypes::{PDFObjRef, PDFStream, dict_value, int_value_obj};
use crate::psparser::{PSKeyword, PSStackParser, PSStackParserImpl};
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::bytes_repr;
use crate::pyrt::io::BytesIO;
use crate::pyrt::value::Value;
use crate::settings;

/// `PDFParser`: fetches PDF objects from a file stream. It can handle indirect references by
/// referring to a PDF document set by `set_document`, and reads the XRefs at the end of the file.
pub struct PDFParser {
    pub ps: PSStackParser,
    pub doc: Option<Weak<PDFDocument>>,
    pub fallback: bool,
}

impl PDFParser {
    pub fn new(fp: BytesIO) -> Self {
        PDFParser { ps: PSStackParser::new(Some(fp)), doc: None, fallback: false }
    }

    /// Associates the parser with a PDFDocument object.
    pub fn set_document(&mut self, doc: Weak<PDFDocument>) {
        self.doc = Some(doc);
    }

    fn fp(&mut self) -> &mut BytesIO {
        self.ps.base.fp.as_mut().expect("parser has a file")
    }

    /// `PDFParser.do_keyword` (also called by subclasses that defer to it).
    pub fn pdfparser_do_keyword(&mut self, pos: i64, token: PSKeyword) -> PyResult<()> {
        if token.is(b"xref") || token.is(b"startxref") {
            let objs = self.ps.pop(1);
            self.ps.add_results(objs);
        } else if token.is(b"endobj") {
            let objs = self.ps.pop(4);
            self.ps.add_results(objs);
        } else if token.is(b"null") {
            // null object
            self.ps.push((pos, Value::None));
        } else if token.is(b"R") {
            // reference to indirect object
            if self.ps.curstack.len() >= 2 {
                let popped = self.ps.pop(2);
                let object_id = safe_int_obj(&popped[0].1)?;
                if let Some(object_id) = object_id {
                    let obj = PDFObjRef::new_value(self.doc.clone(), object_id)?;
                    self.ps.push((pos, Value::Ref(obj)));
                }
            }
        } else if token.is(b"stream") {
            // stream object
            let popped_data = self.ps.pop(1);
            if popped_data.len() != 1 {
                return Err(PyErr::new(
                    ExcKind::PDFSyntaxError,
                    format!("Invalid stream dictionary: {:?}", popped_data),
                ));
            }
            let dic = dict_value(&popped_data[0].1)?;
            let mut objlen_v = Value::Int(0);
            if !self.fallback {
                let length = dic.borrow().get("Length").cloned();
                match length {
                    Some(v) => objlen_v = int_value_obj(&v)?,
                    None => {
                        if settings::STRICT {
                            return Err(PyErr::new(ExcKind::PDFSyntaxError, "/Length is undefined"));
                        }
                    }
                }
            }
            self.seek(pos)?;
            let line = match self.nextline() {
                Ok((_, line)) => line, // 'stream'
                Err(e) if e.is(ExcKind::PSEOF) => {
                    if settings::STRICT {
                        return Err(PyErr::new(ExcKind::PDFSyntaxError, "Unexpected EOF"));
                    }
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            let pos = pos + line.len() as i64;
            self.fp().seek(pos)?;
            // `fp.read(objlen)` of an int beyond i64
            let Some(mut objlen) = objlen_v.as_int() else {
                return Err(PyErr::new(ExcKind::OverflowError, "cannot fit 'int' into an index-sized integer"));
            };
            let mut data: Vec<u8> = self.fp().read(objlen).to_vec();
            self.seek(pos + objlen)?;
            loop {
                let line = match self.nextline() {
                    Ok((_linepos, line)) => line,
                    Err(e) if e.is(ExcKind::PSEOF) => {
                        if settings::STRICT {
                            return Err(PyErr::new(ExcKind::PDFSyntaxError, "Unexpected EOF"));
                        }
                        break;
                    }
                    Err(e) => return Err(e),
                };
                if let Some(i) = find(&line, b"endstream") {
                    objlen += i as i64;
                    if self.fallback {
                        data.extend_from_slice(&line[..i]);
                    }
                    break;
                }
                objlen += line.len() as i64;
                if self.fallback {
                    data.extend_from_slice(&line);
                }
            }
            self.seek(pos + objlen)?;
            // XXX limit objlen not to exceed object boundary
            debug!(
                "Stream: pos={}, objlen={}, dic={:?}, data={}...",
                pos,
                objlen,
                Value::Dict(dic.clone()),
                bytes_repr(&data[..data.len().min(10)])
            );
            let Some(doc) = self.doc.as_ref().and_then(|d| d.upgrade()) else {
                return Err(PyErr::new(ExcKind::AssertionError, ""));
            };
            let stream = PDFStream::new(dic, Rc::from(data), doc.decipher());
            self.ps.push((pos, Value::Stream(Rc::new(stream))));
        } else {
            // others
            self.ps.push((pos, Value::Keyword(token)));
        }
        Ok(())
    }
}

/// `bytes.index(sub)` / `sub in bytes`.
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

impl PSStackParserImpl for PDFParser {
    fn ps(&self) -> &PSStackParser {
        &self.ps
    }
    fn ps_mut(&mut self) -> &mut PSStackParser {
        &mut self.ps
    }
    fn do_keyword(&mut self, pos: i64, token: PSKeyword) -> PyResult<()> {
        self.pdfparser_do_keyword(pos, token)
    }
}

/// `PDFStreamParser`: parses PDF content streams that are contained in each page and have
/// instructions for rendering the page. It needs the document because a content stream can also
/// have indirect references to other objects in the same document.
pub struct PDFStreamParser {
    pub p: PDFParser,
}

impl PDFStreamParser {
    pub fn new(data: &[u8]) -> Self {
        PDFStreamParser { p: PDFParser::new(BytesIO::from_vec(data.to_vec())) }
    }

    pub fn set_document(&mut self, doc: Weak<PDFDocument>) {
        self.p.set_document(doc);
    }
}

impl PSStackParserImpl for PDFStreamParser {
    fn ps(&self) -> &PSStackParser {
        &self.p.ps
    }
    fn ps_mut(&mut self) -> &mut PSStackParser {
        &mut self.p.ps
    }

    fn flush(&mut self) -> PyResult<()> {
        let objs = self.p.ps.popall();
        self.p.ps.add_results(objs);
        Ok(())
    }

    fn do_keyword(&mut self, pos: i64, token: PSKeyword) -> PyResult<()> {
        if token.is(b"R") {
            // reference to indirect object
            // `(_, _object_id), _ = self.pop(2)`: fewer than 2 entries fail to unpack.
            let popped = self.p.ps.pop(2);
            if popped.len() != 2 {
                return Err(PyErr::new(
                    ExcKind::ValueError,
                    format!("not enough values to unpack (expected 2, got {})", popped.len()),
                ));
            }
            let object_id = safe_int_obj(&popped[0].1)?;
            if let Some(object_id) = object_id {
                let obj = PDFObjRef::new_value(self.p.doc.clone(), object_id)?;
                self.p.ps.push((pos, Value::Ref(obj)));
            }
            return Ok(());
        } else if token.is(b"obj") || token.is(b"endobj") {
            if settings::STRICT {
                // See PDF Spec 3.4.6: Only the object values are stored in the
                // stream; the obj and endobj keywords are not used.
                return Err(PyErr::new(ExcKind::PDFSyntaxError, "Keyword endobj found in stream"));
            }
            return Ok(());
        }
        // others
        self.p.ps.push((pos, Value::Keyword(token)));
        Ok(())
    }
}
