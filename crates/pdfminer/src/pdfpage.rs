//! Port of `pdfminer/pdfpage.py`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use log::{debug, warn};

use crate::pdfdocument::{ObjidIter, PDFDocument, PageLabelsIter, RECURSION_LIMIT, py_iter_items};
use crate::pdfparser::PDFParser;
use crate::pdftypes::{dict_value, int_value, list_value, resolve1};
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::float_repr;
use crate::pyrt::io::BytesIO;
use crate::pyrt::value::{DictRef, Value, py_float};
use crate::settings;
use crate::utils::Rect;

// some predefined literals and keywords.
pub const LITERAL_PAGE: &str = "Page";
pub const LITERAL_PAGES: &str = "Pages";

fn is_lit(x: &Value, name: &str) -> bool {
    matches!(x, Value::Literal(l) if l.is(name))
}

fn dget(d: &DictRef, key: &str) -> Option<Value> {
    d.borrow().get(key).cloned()
}

/// An object that holds the information about a page.
///
/// A PDFPage object is merely a convenience class that has a set of keys and values, which
/// describe the properties of a page and point to its contents.
pub struct PDFPage {
    /// a PDFDocument object.
    pub doc: Rc<PDFDocument>,
    /// any Python object that can uniquely identify the page (the object id; `None` for a page
    /// tree node that is a stream without an id).
    pub pageid: Value,
    /// a dictionary of page attributes.
    pub attrs: DictRef,
    /// the page's label (typically, the logical page number).
    pub label: Option<String>,
    /// the last modified time of the page.
    pub lastmod: Value,
    /// a dictionary of resources used by the page (any object, as in Python).
    pub resources: Value,
    /// the physical size of the page.
    pub mediabox: Rect,
    /// the crop rectangle of the page.
    pub cropbox: Rect,
    /// a list of PDFStream objects that represents the page content.
    pub contents: Vec<Value>,
    /// the page rotation (in degree).
    pub rotate: i64,
    /// the page annotations.
    pub annots: Value,
    /// a chain that represents natural reading order.
    pub beads: Value,
}

const US_LETTER: Rect = (0.0, 0.0, 612.0, 792.0);

/// `parse_rect(resolve1(val) for val in resolve1(value))`: `Ok(None)` where Python raises
/// `PDFValueError` (wrong length, or `float()` raising `ValueError`).
fn parse_rect_of(value: &Value) -> PyResult<Option<Rect>> {
    let seq = resolve1(value)?;
    let items = py_iter_items(&seq)?;
    // `(x0, y0, x1, y1) = o` pulls at most five items from the generator.
    let mut o = Vec::with_capacity(4);
    for item in items.iter().take(5) {
        o.push(resolve1(item)?);
    }
    if o.len() != 4 {
        return Ok(None);
    }
    let mut f = [0.0; 4];
    for (i, v) in o.iter().enumerate() {
        match py_float(v) {
            Ok(x) => f[i] = x,
            Err(e) if e.is(ExcKind::ValueError) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
    Ok(Some((f[0], f[1], f[2], f[3])))
}

fn rect_repr(r: &Rect) -> String {
    format!("({}, {}, {}, {})", float_repr(r.0), float_repr(r.1), float_repr(r.2), float_repr(r.3))
}

impl PDFPage {
    /// `PDFPage(doc, pageid, attrs, label)`.
    pub fn new(doc: Rc<PDFDocument>, pageid: Value, attrs: &Value, label: Option<String>) -> PyResult<Self> {
        let attrs = dict_value(attrs)?;
        let lastmod = resolve1(&dget(&attrs, "LastModified").unwrap_or(Value::None))?;
        let resources = resolve1(&dget(&attrs, "Resources").unwrap_or_else(|| Value::Dict(Value::new_dict())))?;
        let mediabox = Self::_parse_mediabox(dget(&attrs, "MediaBox"))?;
        let cropbox = Self::_parse_cropbox(dget(&attrs, "CropBox"), mediabox)?;
        let contents = Self::_parse_contents(dget(&attrs, "Contents"))?;
        let rotate = int_value(&dget(&attrs, "Rotate").unwrap_or(Value::Int(0)))?;
        // (rotate + 360) % 360, without overflowing i64.
        let rotate = (rotate as i128 + 360).rem_euclid(360) as i64;
        let annots = dget(&attrs, "Annots").unwrap_or(Value::None);
        let beads = dget(&attrs, "B").unwrap_or(Value::None);
        Ok(PDFPage {
            doc,
            pageid,
            attrs,
            label,
            lastmod,
            resources,
            mediabox,
            cropbox,
            contents,
            rotate,
            annots,
            beads,
        })
    }

    pub fn repr(&self) -> String {
        format!("<PDFPage: Resources={}, MediaBox={}>", self.resources.repr(), rect_repr(&self.mediabox))
    }

    /// `INHERITABLE_ATTRS`.
    pub const INHERITABLE_ATTRS: [&'static str; 4] = ["Resources", "MediaBox", "CropBox", "Rotate"];

    /// `PDFPage.create_pages(document)`: the pages, lazily (an error on page 5 comes after pages
    /// 1-4, as from Python's generator).
    pub fn create_pages(document: &Rc<PDFDocument>) -> CreatePages {
        CreatePages {
            doc: document.clone(),
            started: false,
            page_labels: None,
            stack: Vec::new(),
            pending: None,
            visited: HashSet::new(),
            phase: Phase::Tree,
            pages: false,
            xref_idx: 0,
            objids: None,
            done: false,
        }
    }

    /// `PDFPage.get_pages(fp, pagenos, maxpages, password, caching, check_extractable)`: `fp` is
    /// the file's bytes (opened like a real file: a negative seek raises `OSError`). Lazy: the
    /// document is only opened by the first `next()`, as in Python.
    pub fn get_pages(
        fp: Rc<[u8]>,
        pagenos: Option<HashSet<i64>>,
        maxpages: i64,
        password: &str,
        caching: bool,
        check_extractable: bool,
    ) -> GetPages {
        GetPages {
            fp: Some(fp),
            pagenos,
            maxpages,
            password: password.to_string(),
            caching,
            check_extractable,
            pages: None,
            pageno: 0,
            done: false,
        }
    }

    fn _parse_mediabox(value: Option<Value>) -> PyResult<Rect> {
        let value = match value {
            None | Some(Value::None) => {
                warn!("MediaBox missing from /Page (and not inherited), defaulting to US Letter");
                return Ok(US_LETTER);
            }
            Some(v) => v,
        };
        match parse_rect_of(&value)? {
            Some(r) => Ok(r),
            None => {
                warn!("Invalid MediaBox in /Page, defaulting to US Letter");
                Ok(US_LETTER)
            }
        }
    }

    fn _parse_cropbox(value: Option<Value>, mediabox: Rect) -> PyResult<Rect> {
        let value = match value {
            // CropBox is optional, and MediaBox is used if not specified.
            None | Some(Value::None) => return Ok(mediabox),
            Some(v) => v,
        };
        match parse_rect_of(&value)? {
            Some(r) => Ok(r),
            None => {
                warn!("Invalid CropBox in /Page, defaulting to MediaBox");
                Ok(mediabox)
            }
        }
    }

    fn _parse_contents(value: Option<Value>) -> PyResult<Vec<Value>> {
        let mut contents: Vec<Value> = Vec::new();
        if let Some(value) = value.filter(|v| !v.is_none()) {
            let c = resolve1(&value)?;
            contents = match &c {
                Value::List(l) => l.borrow().clone(),
                _ => vec![c],
            };
        }
        Ok(contents)
    }
}

enum Labels {
    Labels(PageLabelsIter),
    /// `itertools.repeat(None)`
    Repeat,
}

enum Phase {
    Tree,
    Fallback,
}

/// A page tree node of `depth_first_search`: its kids, the next kid, and its (inherited)
/// properties.
struct Frame {
    kids: Vec<Value>,
    idx: usize,
    props: DictRef,
}

/// The generator `PDFPage.create_pages(document)` returns.
pub struct CreatePages {
    doc: Rc<PDFDocument>,
    started: bool,
    page_labels: Option<Labels>,
    stack: Vec<Frame>,
    /// A node to visit next: `(obj, parent)`.
    pending: Option<(Value, DictRef)>,
    visited: HashSet<Option<i64>>,
    phase: Phase,
    pages: bool,
    xref_idx: usize,
    objids: Option<ObjidIter>,
    done: bool,
}

impl CreatePages {
    /// `next(page_labels)`.
    fn next_label(&mut self) -> PyResult<Option<String>> {
        match self.page_labels.as_mut().expect("started") {
            Labels::Repeat => Ok(None),
            Labels::Labels(it) => match it.next() {
                Some(Ok(s)) => Ok(Some(s)),
                Some(Err(e)) => Err(e),
                // PEP 479: StopIteration inside a generator.
                None => Err(PyErr::new(ExcKind::RuntimeError, "generator raised StopIteration")),
            },
        }
    }

    /// Enters one node of `depth_first_search(obj, parent, visited)`; returns a page to yield.
    fn enter(&mut self, obj: Value, parent: DictRef) -> PyResult<Option<(Value, DictRef)>> {
        let copy = |d: DictRef| Rc::new(RefCell::new(d.borrow().clone()));
        let obj_is_int = obj.is_int();
        let obj_value = obj.clone();
        let (object_id, object_properties): (Option<i64>, DictRef) = if let Some(id) = obj.as_int() {
            (Some(id), copy(dict_value(&self.doc.getobj_value(&obj)?)?))
        } else {
            // This looks broken. obj.objid means obj could be either
            // PDFObjRef or PDFStream, but neither is valid for dict_value.
            let object_id = match &obj {
                Value::Ref(r) => Some(r.objid()),
                Value::Stream(s) => s.objid.get(),
                other => {
                    return Err(PyErr::new(
                        ExcKind::AttributeError,
                        format!("'{}' object has no attribute 'objid'", other.type_name()),
                    ));
                }
            };
            (object_id, copy(dict_value(&obj)?))
        };

        // Avoid recursion errors by keeping track of visited nodes
        if self.visited.contains(&object_id) {
            return Ok(None);
        }
        self.visited.insert(object_id);

        let parent_items: Vec<(Rc<str>, Value)> = parent.borrow().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (k, v) in parent_items {
            if PDFPage::INHERITABLE_ATTRS.contains(&&*k) && !object_properties.borrow().contains_key(&k) {
                object_properties.borrow_mut().insert(k, v);
            }
        }

        let mut object_type = dget(&object_properties, "Type");
        if object_type.is_none() && !settings::STRICT {
            // See #64
            object_type = dget(&object_properties, "type");
        }
        let object_type = object_type.unwrap_or(Value::None);

        // `object_id` itself (an int object from the Kids array stays as it was, e.g. `True`).
        let pageid = if obj_is_int { obj_value } else { object_id.map_or(Value::None, Value::Int) };
        if is_lit(&object_type, LITERAL_PAGES) && object_properties.borrow().contains_key("Kids") {
            let kids = dget(&object_properties, "Kids").unwrap();
            debug!("Pages: Kids={}", kids.repr());
            let kids = list_value(&kids)?;
            self.stack.push(Frame { kids, idx: 0, props: object_properties });
        } else if is_lit(&object_type, LITERAL_PAGE) {
            debug!("Page: {}", Value::Dict(object_properties.clone()).repr());
            return Ok(Some((pageid, object_properties)));
        }
        Ok(None)
    }

    fn step(&mut self) -> PyResult<Option<PDFPage>> {
        if !self.started {
            self.started = true;
            self.page_labels = Some(match self.doc.get_page_labels() {
                Ok(it) => Labels::Labels(it),
                Err(e) if e.is(ExcKind::PDFNoPageLabels) => Labels::Repeat,
                Err(e) => return Err(e),
            });
            let catalog = self.doc.catalog();
            match dget(&catalog, "Pages") {
                Some(pages) => self.pending = Some((pages, catalog)),
                None => self.phase = Phase::Fallback,
            }
        }
        if let Phase::Tree = self.phase {
            loop {
                if let Some((obj, parent)) = self.pending.take() {
                    // A nested generator for every level of the tree.
                    if self.stack.len() >= RECURSION_LIMIT {
                        return Err(PyErr::new(ExcKind::RecursionError, "maximum recursion depth exceeded"));
                    }
                    if let Some((objid, tree)) = self.enter(obj, parent)? {
                        let label = self.next_label()?;
                        let page = PDFPage::new(self.doc.clone(), objid, &Value::Dict(tree), label)?;
                        self.pages = true;
                        return Ok(Some(page));
                    }
                    continue;
                }
                let Some(top) = self.stack.last_mut() else { break };
                if top.idx < top.kids.len() {
                    let child = top.kids[top.idx].clone();
                    top.idx += 1;
                    self.pending = Some((child, top.props.clone()));
                } else {
                    self.stack.pop();
                }
            }
            if self.pages {
                return Ok(None);
            }
            self.phase = Phase::Fallback;
        }
        // fallback when /Pages is missing.
        let xrefs = self.doc.xrefs();
        loop {
            if self.objids.is_none() {
                if self.xref_idx >= xrefs.len() {
                    return Ok(None);
                }
                self.objids = Some(xrefs[self.xref_idx].get_objids());
                self.xref_idx += 1;
            }
            let objid = match self.objids.as_mut().unwrap().next() {
                Some(r) => r?,
                None => {
                    self.objids = None;
                    continue;
                }
            };
            let r = (|| -> PyResult<Option<PDFPage>> {
                let obj = self.doc.getobj_value(&objid)?;
                if let Value::Dict(d) = &obj
                    && dget(d, "Type").is_some_and(|t| is_lit(&t, LITERAL_PAGE))
                {
                    let label = self.next_label()?;
                    return Ok(Some(PDFPage::new(self.doc.clone(), objid.clone(), &obj, label)?));
                }
                Ok(None)
            })();
            match r {
                Ok(Some(page)) => return Ok(Some(page)),
                Ok(None) => {}
                Err(e) if e.is(ExcKind::PDFObjectNotFound) => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl Iterator for CreatePages {
    type Item = PyResult<PDFPage>;

    fn next(&mut self) -> Option<PyResult<PDFPage>> {
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

/// The generator `PDFPage.get_pages(...)` returns.
pub struct GetPages {
    fp: Option<Rc<[u8]>>,
    pagenos: Option<HashSet<i64>>,
    maxpages: i64,
    password: String,
    caching: bool,
    check_extractable: bool,
    pages: Option<CreatePages>,
    pageno: i64,
    done: bool,
}

impl GetPages {
    /// The document, once the first `next()` has opened it.
    pub fn doc(&self) -> Option<Rc<PDFDocument>> {
        self.pages.as_ref().map(|p| p.doc.clone())
    }

    fn step(&mut self) -> PyResult<Option<PDFPage>> {
        if let Some(fp) = self.fp.take() {
            // Create a PDF parser object associated with the file object.
            let parser = PDFParser::new(BytesIO::file(fp));
            // Create a PDF document object that stores the document structure.
            let doc = PDFDocument::new(parser, &self.password, self.caching, true)?;
            // Check if the document allows text extraction.
            // If not, warn the user and proceed.
            if !doc.is_extractable() {
                if self.check_extractable {
                    // DIVERGE(fp-repr): Python's message includes `repr(fp)`.
                    let error_msg = "Text extraction is not allowed: <file>";
                    return Err(PyErr::new(ExcKind::PDFTextExtractionNotAllowed, error_msg));
                } else {
                    warn!(
                        "The PDF <file> contains a metadata field indicating that it should not allow text \
                         extraction. Ignoring this field and proceeding. Use the check_extractable if you \
                         want to raise an error in this case"
                    );
                }
            }
            self.pages = Some(PDFPage::create_pages(&doc));
        }
        // Process each page contained in the document.
        let Some(pages) = self.pages.as_mut() else { return Ok(None) };
        loop {
            let page = match pages.next() {
                Some(r) => r?,
                None => return Ok(None),
            };
            let pageno = self.pageno;
            self.pageno += 1;
            if let Some(pagenos) = &self.pagenos
                && !pagenos.is_empty()
                && !pagenos.contains(&pageno)
            {
                continue;
            }
            if self.maxpages != 0 && self.maxpages <= pageno + 1 {
                // `yield page` and then `break`.
                self.done = true;
            }
            return Ok(Some(page));
        }
    }
}

impl Iterator for GetPages {
    type Item = PyResult<PDFPage>;

    fn next(&mut self) -> Option<PyResult<PDFPage>> {
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
