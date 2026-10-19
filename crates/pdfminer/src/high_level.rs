//! Port of `pdfminer/high_level.py`.
//!
//! Functions that can be used for the most common use-cases for pdfminer.six

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crate::converter::{
    HOCRConverter, HTMLConverter, HTMLConverterParams, Num, PDFPageAggregator, TextConverter, XMLConverter,
};
use crate::image::ImageWriter;
use crate::layout::{LAParams, LTItem};
use crate::pdfdevice::{PDFDevice, TagExtractor};
use crate::pdfinterp::{PDFPageInterpreter, PDFResourceManager};
use crate::pdfpage::{GetPages, PDFPage};
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::pystr::PyStr;
use crate::pyrt::textio::{AnyIO, StringIO, os_error_path};

/// `FileOrName`: a path, or the content of an open binary file.
#[derive(Clone)]
pub enum FileOrName {
    Path(String),
    Data(Rc<[u8]>),
}

impl From<&str> for FileOrName {
    fn from(p: &str) -> Self {
        FileOrName::Path(p.to_string())
    }
}

impl From<Vec<u8>> for FileOrName {
    fn from(d: Vec<u8>) -> Self {
        FileOrName::Data(d.into())
    }
}

/// `open(path, "rb").read()`, with Python's `OSError`s.
pub fn read_file(path: &str) -> PyResult<Rc<[u8]>> {
    std::fs::read(path).map(Rc::from).map_err(|e| os_error_path(&e, path))
}

/// `open_filename(pdf_file, "rb")`.
fn open_filename(pdf_file: &FileOrName) -> PyResult<Rc<[u8]>> {
    match pdf_file {
        FileOrName::Path(p) => read_file(p),
        FileOrName::Data(d) => Ok(d.clone()),
    }
}

/// The keyword arguments of `extract_text_to_fp`, with Python's defaults.
#[derive(Clone)]
pub struct ExtractTextToFp {
    pub output_type: String,
    pub codec: String,
    pub laparams: Option<LAParams>,
    pub maxpages: i64,
    pub page_numbers: Option<HashSet<i64>>,
    pub password: String,
    pub scale: f64,
    pub rotation: i64,
    pub layoutmode: String,
    pub output_dir: Option<String>,
    pub strip_control: bool,
    pub debug: bool,
    pub disable_caching: bool,
}

impl Default for ExtractTextToFp {
    fn default() -> Self {
        ExtractTextToFp {
            output_type: "text".into(),
            codec: "utf-8".into(),
            laparams: None,
            maxpages: 0,
            page_numbers: None,
            password: String::new(),
            scale: 1.0,
            rotation: 0,
            layoutmode: "normal".into(),
            output_dir: None,
            strip_control: false,
            debug: false,
            disable_caching: false,
        }
    }
}

/// Parses text from inf-file and writes to outfp file-like object.
///
/// `stdout_buffer` is `Some(sys.stdout.buffer)` when `outfp` is `sys.stdout` (Python then
/// writes every output type but text to the binary buffer).
///
/// Takes loads of optional arguments but the defaults are somewhat sane. Beware laparams:
/// Including an empty LAParams is not the same as passing None!
pub fn extract_text_to_fp(
    inf: Rc<[u8]>,
    outfp: AnyIO,
    stdout_buffer: Option<AnyIO>,
    args: ExtractTextToFp,
) -> PyResult<()> {
    let ExtractTextToFp {
        output_type,
        codec,
        laparams,
        maxpages,
        page_numbers,
        password,
        scale,
        rotation,
        layoutmode,
        output_dir,
        strip_control,
        debug: _,
        disable_caching,
    } = args;

    let imagewriter = match output_dir.as_deref() {
        Some(d) if !d.is_empty() => Some(ImageWriter::new(d)?),
        _ => None,
    };

    let rsrcmgr = PDFResourceManager::new(!disable_caching);

    let mut outfp = outfp;
    if output_type != "text"
        && let Some(buffer) = stdout_buffer
    {
        outfp = buffer;
    }

    let device: Rc<RefCell<dyn PDFDevice>> = if output_type == "text" {
        Rc::new(RefCell::new(TextConverter::new(rsrcmgr.clone(), outfp, &codec, 1, laparams, false, imagewriter)))
    } else if output_type == "xml" {
        Rc::new(RefCell::new(XMLConverter::new(
            rsrcmgr.clone(),
            outfp,
            &codec,
            1,
            laparams,
            imagewriter,
            strip_control,
        )?))
    } else if output_type == "html" {
        let p = HTMLConverterParams {
            codec: codec.clone(),
            scale: Num::Float(scale),
            layoutmode,
            laparams,
            imagewriter,
            ..Default::default()
        };
        Rc::new(RefCell::new(HTMLConverter::new(rsrcmgr.clone(), outfp, p)?))
    } else if output_type == "hocr" {
        Rc::new(RefCell::new(HOCRConverter::new(rsrcmgr.clone(), outfp, &codec, 1, laparams, strip_control)?))
    } else if output_type == "tag" {
        // Binary I/O is required, but we have no good way to test it here.
        Rc::new(RefCell::new(TagExtractor::new(outfp, &codec)))
    } else {
        let msg = format!("Output type can be text, html, xml or tag but is {output_type}");
        return Err(PyErr::new(ExcKind::PDFValueError, msg));
    };

    let mut interpreter = PDFPageInterpreter::new(rsrcmgr, device.clone());
    for page in PDFPage::get_pages(inf, page_numbers, maxpages, &password, !disable_caching, false) {
        let mut page = page?;
        page.rotate = (page.rotate as i128 + rotation as i128).rem_euclid(360) as i64;
        interpreter.process_page(&page)?;
    }

    device.borrow_mut().close()
}

/// Parse and return the text contained in a PDF file.
///
/// `laparams`: `None` uses some default settings that often work well.
pub fn extract_text(
    pdf_file: &FileOrName,
    password: &str,
    page_numbers: Option<HashSet<i64>>,
    maxpages: i64,
    caching: bool,
    codec: &str,
    laparams: Option<LAParams>,
) -> PyResult<PyStr> {
    let laparams = laparams.unwrap_or_default();

    let fp = open_filename(pdf_file)?;
    let output_string = Rc::new(RefCell::new(StringIO::new()));
    let rsrcmgr = PDFResourceManager::new(caching);
    let device = Rc::new(RefCell::new(TextConverter::new(
        rsrcmgr.clone(),
        output_string.clone(),
        codec,
        1,
        Some(laparams),
        false,
        None,
    )));
    let mut interpreter = PDFPageInterpreter::new(rsrcmgr, device);

    for page in PDFPage::get_pages(fp, page_numbers, maxpages, password, caching, false) {
        interpreter.process_page(&page?)?;
    }

    let s = output_string.borrow().getvalue();
    Ok(s)
}

/// Extract and yield LTPage objects: a lazy iterator, like Python's generator (the file is
/// opened at the first `next()`, and an error comes after the pages before it).
///
/// `laparams`: `None` uses some default settings that often work well.
pub fn extract_pages(
    pdf_file: FileOrName,
    password: &str,
    page_numbers: Option<HashSet<i64>>,
    maxpages: i64,
    caching: bool,
    laparams: Option<LAParams>,
) -> ExtractPages {
    ExtractPages {
        start: Some((pdf_file, password.to_string(), page_numbers, maxpages, caching, laparams.unwrap_or_default())),
        running: None,
        done: false,
    }
}

type Running = (Rc<RefCell<PDFPageAggregator>>, PDFPageInterpreter, GetPages);

/// The generator `extract_pages` returns.
pub struct ExtractPages {
    start: Option<(FileOrName, String, Option<HashSet<i64>>, i64, bool, LAParams)>,
    running: Option<Running>,
    done: bool,
}

impl ExtractPages {
    /// The aggregator without layout analysis (`PDFPageAggregator(rsrcmgr, laparams=None)` over
    /// `PDFPage.get_pages(fp, password=password)`), as `tools/oracle.py layout --laparams none`
    /// runs it. Not part of pdfminer's API.
    pub fn without_laparams(data: Rc<[u8]>, password: &str) -> ExtractPages {
        let rsrcmgr = PDFResourceManager::new(true);
        let device = Rc::new(RefCell::new(PDFPageAggregator::new(rsrcmgr.clone(), 1, None)));
        let interpreter = PDFPageInterpreter::new(rsrcmgr, device.clone());
        let pages = PDFPage::get_pages(data, None, 0, password, true, false);
        ExtractPages { start: None, running: Some((device, interpreter, pages)), done: false }
    }

    fn step(&mut self) -> PyResult<Option<LTItem>> {
        if let Some((pdf_file, password, page_numbers, maxpages, caching, laparams)) = self.start.take() {
            let fp = open_filename(&pdf_file)?;
            let resource_manager = PDFResourceManager::new(caching);
            let device = Rc::new(RefCell::new(PDFPageAggregator::new(resource_manager.clone(), 1, Some(laparams))));
            let interpreter = PDFPageInterpreter::new(resource_manager, device.clone());
            let pages = PDFPage::get_pages(fp, page_numbers, maxpages, &password, caching, false);
            self.running = Some((device, interpreter, pages));
        }
        let Some((device, interpreter, pages)) = self.running.as_mut() else {
            return Ok(None);
        };
        match pages.next() {
            None => Ok(None),
            Some(page) => {
                let page = page?;
                interpreter.process_page(&page)?;
                let layout = device.borrow().get_result()?;
                Ok(Some(layout))
            }
        }
    }
}

impl Iterator for ExtractPages {
    type Item = PyResult<LTItem>;

    fn next(&mut self) -> Option<PyResult<LTItem>> {
        if self.done {
            return None;
        }
        match self.step() {
            Ok(Some(p)) => Some(Ok(p)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
