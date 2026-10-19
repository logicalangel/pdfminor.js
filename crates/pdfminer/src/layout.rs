//! Port of `pdfminer/layout.py`.
//!
//! Python's layout classes form a closed hierarchy that callers inspect with `isinstance`, share
//! by reference (a `Plane` holds the same objects as the page, `group_textboxes` keys a heap by
//! `id(obj)`) and mutate during the analysis (`add` grows a container's bbox, `analyze` replaces
//! a page's children). The port keeps that object model:
//!
//! - [`LTItem`] is a reference to a layout object (`Rc`): cloning it copies the reference, and
//!   [`LTItem::is`] is Python's `is`. Hash and `==` are identity, like Python's default.
//! - [`LTClass`] is the object's class; `is_<class>()` helpers follow Python's MRO, so
//!   `is_lttextline()` is true for `LTTextLineHorizontal`.
//! - The mutable state (bbox, children, `index`, `groups`) sits in `Cell`/`RefCell`; the
//!   per-class attributes that never change after construction are plain fields
//!   ([`LTCharAttrs`], [`LTCurveAttrs`], [`LTImageAttrs`], [`LTFigureAttrs`], [`LTPageAttrs`]).
//! - The constructors keep Python's names and signatures: `LTChar(...)`, `LTPage(...)`, ...
//!
//! Two Python behaviours need a rule of their own:
//!
//! - `id(obj)` breaks ties in `group_textboxes`' heap. CPython's ids are memory addresses, so the
//!   order of tied pairs depends on the allocator; pdfminer.six itself gives different groupings
//!   between runs on some files. The port uses the creation order of the objects (every layout
//!   object gets a serial number when it is created), which is what CPython's addresses follow
//!   most of the time. DIVERGE(heap-id-ties).
//! - Python's recursion limit: `analyze` recurses through text groups, whose depth is up to the
//!   number of text boxes. Python raises `RecursionError` when the frames on the stack exceed
//!   1000; the port counts the same frames, assuming `LTPage.analyze` runs at frame depth
//!   [`ANALYZE_FRAME_DEPTH`] as it does under `extract_pages` and `pdf2txt.py`.
//!   DIVERGE(recursion-depth).

#![allow(non_snake_case)]

use std::cell::{Cell, Ref, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::pdfcolor::PDFColorSpace;
use crate::pdffont::PDFFont;
use crate::pdfinterp::{Color, PDFGraphicStateRef};
use crate::pdftypes::PDFStream;
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::{fixed, float_repr, pystr_repr};
use crate::pyrt::heapq;
use crate::pyrt::pystr::PyStr;
use crate::pyrt::sort::{sort_by_f64_key, sort_by_key_lt};
use crate::pyrt::value::{Value, py_mul, py_sub, pymax, pymin};
use crate::utils::{INF, Matrix, Plane, Point, Rect, apply_matrix_rect, bbox2str, fsplit, get_bound, matrix2str, uniq};

/// `sys.getrecursionlimit()` (CPython's default).
pub const PY_RECURSION_LIMIT: usize = 1000;

/// The Python frame depth at which `LTPage.analyze` runs under `extract_pages` and `pdf2txt.py`
/// (`<module>`, `main`, `extract_text`/`cmd_layout`, ..., `process_page`, `end_page`, `analyze`).
/// DIVERGE(recursion-depth): Python's depth depends on the caller's stack.
pub const ANALYZE_FRAME_DEPTH: usize = 7;

/// Python pushes a frame at `depth`: `RecursionError` beyond the limit.
// DIVERGE(recursion-depth): frames are counted from `ANALYZE_FRAME_DEPTH`.
fn enter_frame(depth: usize) -> PyResult<()> {
    if depth > PY_RECURSION_LIMIT {
        return Err(PyErr::new(ExcKind::RecursionError, "maximum recursion depth exceeded"));
    }
    Ok(())
}

/// `IndexAssigner`: numbers the text boxes of a group tree in order.
pub struct IndexAssigner {
    pub index: i64,
}

impl IndexAssigner {
    pub fn new(index: i64) -> Self {
        IndexAssigner { index }
    }

    pub fn run(&mut self, obj: &LTItem) -> PyResult<()> {
        self.run_at(obj, ANALYZE_FRAME_DEPTH + 1)
    }

    /// `run(obj)` as a Python frame at `depth`.
    pub fn run_at(&mut self, obj: &LTItem, depth: usize) -> PyResult<()> {
        enter_frame(depth)?;
        if obj.is_lttextbox() {
            obj.set_index(self.index);
            self.index += 1;
        } else if obj.is_lttextgroup() {
            for x in obj.objs() {
                self.run_at(&x, depth + 1)?;
            }
        }
        Ok(())
    }
}

/// Parameters for layout analysis
///
/// - `line_overlap`: If two characters have more overlap than this they are considered to be on
///   the same line. The overlap is specified relative to the minimum height of both characters.
/// - `char_margin`: If two characters are closer together than this margin they are considered
///   part of the same line. The margin is specified relative to the width of the character.
/// - `word_margin`: If two characters on the same line are further apart than this margin then
///   they are considered to be two separate words, and an intermediate space will be added for
///   readability. The margin is specified relative to the width of the character.
/// - `line_margin`: If two lines are are close together they are considered to be part of the
///   same paragraph. The margin is specified relative to the height of a line.
/// - `boxes_flow`: Specifies how much a horizontal and vertical position of a text matters when
///   determining the order of text boxes. The value should be within the range of -1.0 (only
///   horizontal position matters) to +1.0 (only vertical position matters). You can also pass
///   `None` to disable advanced layout analysis, and instead return text based on the position
///   of the bottom left corner of the text box.
/// - `detect_vertical`: If vertical text should be considered during layout analysis
/// - `all_texts`: If layout analysis should be performed on text in figures.
#[derive(Clone, Debug, PartialEq)]
pub struct LAParams {
    pub line_overlap: f64,
    pub char_margin: f64,
    pub line_margin: f64,
    pub word_margin: f64,
    pub boxes_flow: Option<f64>,
    pub detect_vertical: bool,
    pub all_texts: bool,
}

const BOXES_FLOW_ERR_MSG: &str = "LAParam boxes_flow should be None, or a number between -1 and +1";

impl Default for LAParams {
    fn default() -> Self {
        LAParams {
            line_overlap: 0.5,
            char_margin: 2.0,
            line_margin: 0.5,
            word_margin: 0.1,
            boxes_flow: Some(0.5),
            detect_vertical: false,
            all_texts: false,
        }
    }
}

impl LAParams {
    /// `LAParams(line_overlap, char_margin, line_margin, word_margin, boxes_flow,
    /// detect_vertical, all_texts)`.
    pub fn new(
        line_overlap: f64,
        char_margin: f64,
        line_margin: f64,
        word_margin: f64,
        boxes_flow: Option<f64>,
        detect_vertical: bool,
        all_texts: bool,
    ) -> PyResult<Self> {
        let s =
            LAParams { line_overlap, char_margin, line_margin, word_margin, boxes_flow, detect_vertical, all_texts };
        s._validate()?;
        Ok(s)
    }

    pub fn _validate(&self) -> PyResult<()> {
        if let Some(boxes_flow) = self.boxes_flow {
            // `isinstance(self.boxes_flow, (int, float))` holds for an f64; see
            // `boxes_flow_from_value` for untyped input.
            if !(-1.0..=1.0).contains(&boxes_flow) {
                return Err(PyErr::new(ExcKind::PDFValueError, BOXES_FLOW_ERR_MSG));
            }
        }
        Ok(())
    }

    /// The type check of `_validate` for a `boxes_flow` given as an untyped Python object (the
    /// bindings): `PDFTypeError` unless it is `None` or a number.
    pub fn boxes_flow_from_value(v: &Value) -> PyResult<Option<f64>> {
        match v {
            Value::None => Ok(None),
            _ if v.is_number() => Ok(v.as_f64()),
            _ => Err(PyErr::new(ExcKind::PDFTypeError, BOXES_FLOW_ERR_MSG)),
        }
    }

    pub fn repr(&self) -> String {
        format!(
            "<LAParams: char_margin={}, line_margin={}, word_margin={} all_texts={}>",
            fixed(self.char_margin, 1),
            fixed(self.line_margin, 1),
            fixed(self.word_margin, 1),
            if self.all_texts { "True" } else { "False" }
        )
    }
}

// ---------------------------------------------------------------------------------------------
// The object model

/// The class of a layout object. `LTItem` and `LTText` are interfaces in Python and never
/// instantiated by pdfminer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LTClass {
    LTComponent,
    LTCurve,
    LTLine,
    LTRect,
    LTImage,
    LTAnno,
    LTChar,
    LTContainer,
    LTExpandableContainer,
    LTTextContainer,
    LTTextLine,
    LTTextLineHorizontal,
    LTTextLineVertical,
    LTTextBox,
    LTTextBoxHorizontal,
    LTTextBoxVertical,
    LTTextGroup,
    LTTextGroupLRTB,
    LTTextGroupTBRL,
    LTLayoutContainer,
    LTFigure,
    LTPage,
}

impl LTClass {
    /// `cls.__name__`.
    pub fn name(self) -> &'static str {
        use LTClass::*;
        match self {
            LTComponent => "LTComponent",
            LTCurve => "LTCurve",
            LTLine => "LTLine",
            LTRect => "LTRect",
            LTImage => "LTImage",
            LTAnno => "LTAnno",
            LTChar => "LTChar",
            LTContainer => "LTContainer",
            LTExpandableContainer => "LTExpandableContainer",
            LTTextContainer => "LTTextContainer",
            LTTextLine => "LTTextLine",
            LTTextLineHorizontal => "LTTextLineHorizontal",
            LTTextLineVertical => "LTTextLineVertical",
            LTTextBox => "LTTextBox",
            LTTextBoxHorizontal => "LTTextBoxHorizontal",
            LTTextBoxVertical => "LTTextBoxVertical",
            LTTextGroup => "LTTextGroup",
            LTTextGroupLRTB => "LTTextGroupLRTB",
            LTTextGroupTBRL => "LTTextGroupTBRL",
            LTLayoutContainer => "LTLayoutContainer",
            LTFigure => "LTFigure",
            LTPage => "LTPage",
        }
    }

    /// The class this one derives from (besides the `LTItem`/`LTText` interfaces).
    pub fn base(self) -> Option<LTClass> {
        use LTClass::*;
        match self {
            LTComponent | LTAnno => None,
            LTCurve | LTImage | LTChar | LTContainer => Some(LTComponent),
            LTLine | LTRect => Some(LTCurve),
            LTExpandableContainer | LTLayoutContainer => Some(LTContainer),
            LTTextContainer => Some(LTExpandableContainer),
            LTTextLine | LTTextBox | LTTextGroup => Some(LTTextContainer),
            LTTextLineHorizontal | LTTextLineVertical => Some(LTTextLine),
            LTTextBoxHorizontal | LTTextBoxVertical => Some(LTTextBox),
            LTTextGroupLRTB | LTTextGroupTBRL => Some(LTTextGroup),
            LTFigure | LTPage => Some(LTLayoutContainer),
        }
    }

    /// `issubclass(self, other)`.
    pub fn is_subclass_of(self, other: LTClass) -> bool {
        let mut c = Some(self);
        while let Some(k) = c {
            if k == other {
                return true;
            }
            c = k.base();
        }
        false
    }

    /// `issubclass(self, LTText)`.
    pub fn is_text(self) -> bool {
        matches!(self, LTClass::LTAnno | LTClass::LTChar) || self.is_subclass_of(LTClass::LTTextContainer)
    }
}

/// `PathSegment` of a curve's `original_path`: the operator and its points, already
/// transformed by the CTM (`converter.paint_path` builds `(operator, (x, y), ...)` tuples).
pub type PathSegment = (String, Vec<Point>);

/// The attributes of an `LTChar`.
pub struct LTCharAttrs {
    _text: PyStr,
    pub matrix: Matrix,
    /// `font.fontname`, the Python object (usually a `str`).
    pub fontname: Value,
    pub ncs: PDFColorSpace,
    /// The interpreter's graphic state object itself (Python keeps a live reference).
    pub graphicstate: PDFGraphicStateRef,
    pub adv: f64,
    pub upright: bool,
    pub size: f64,
}

/// The attributes of an `LTCurve` (and `LTLine`, `LTRect`).
pub struct LTCurveAttrs {
    pub pts: Vec<Point>,
    /// `gstate.linewidth` as Python had it (an `int` prints as `0`, a `float` as `0.0`).
    pub linewidth: Value,
    pub stroke: bool,
    pub fill: bool,
    pub evenodd: bool,
    pub stroking_color: Option<Color>,
    pub non_stroking_color: Option<Color>,
    pub original_path: Option<Vec<PathSegment>>,
    /// `gstate.dash`: `(dash, phase)` or `None`.
    pub dashing_style: Option<(Value, Value)>,
}

impl LTCurveAttrs {
    /// `get_pts()`.
    pub fn get_pts(&self) -> String {
        let parts: Vec<String> = self.pts.iter().map(|&(x, y)| format!("{},{}", fixed(x, 3), fixed(y, 3))).collect();
        parts.join(",")
    }
}

/// The attributes of an `LTImage`.
pub struct LTImageAttrs {
    pub name: String,
    pub stream: Rc<PDFStream>,
    /// `(stream.get_any(("W", "Width")), stream.get_any(("H", "Height")))`.
    pub srcsize: (Value, Value),
    pub imagemask: Value,
    pub bits: Value,
    /// Always a Python list: the stream's own list object, or `[colorspace]`.
    pub colorspace: Value,
}

/// The attributes of an `LTFigure`.
pub struct LTFigureAttrs {
    pub name: String,
    pub matrix: Matrix,
}

/// The attributes of an `LTPage`.
pub struct LTPageAttrs {
    pub pageid: i64,
    /// `rotate` as Python had it (`0` by default, an `int`).
    pub rotate: Value,
}

#[derive(Clone, Copy)]
struct Geom {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    width: f64,
    height: f64,
}

impl Geom {
    /// `LTComponent.set_bbox(bbox)`.
    fn from_bbox(bbox: Rect) -> Geom {
        let (x0, y0, x1, y1) = bbox;
        Geom { x0, y0, x1, y1, width: x1 - x0, height: y1 - y0 }
    }
}

/// The state of a container: its children and the attributes its subclasses add.
struct ContainerAttrs {
    _objs: RefCell<Vec<LTItem>>,
    /// `LTTextLine.word_margin`.
    word_margin: f64,
    /// `LTTextLineHorizontal._x1` / `LTTextLineVertical._y0`.
    edge: Cell<f64>,
    /// `LTTextBox.index`.
    index: Cell<i64>,
    /// `LTLayoutContainer.groups`.
    groups: RefCell<Option<Vec<LTItem>>>,
    figure: Option<LTFigureAttrs>,
    page: Option<LTPageAttrs>,
}

impl ContainerAttrs {
    fn new() -> Self {
        ContainerAttrs {
            _objs: RefCell::new(Vec::new()),
            word_margin: 0.0,
            edge: Cell::new(0.0),
            index: Cell::new(-1),
            groups: RefCell::new(None),
            figure: None,
            page: None,
        }
    }
}

enum Attrs {
    None,
    Anno(PyStr),
    Char(LTCharAttrs),
    Curve(Box<LTCurveAttrs>),
    Image(Box<LTImageAttrs>),
    Container(Box<ContainerAttrs>),
}

/// A layout object (the node behind an [`LTItem`] reference).
pub struct LTNode {
    serial: u64,
    class: LTClass,
    geom: Cell<Geom>,
    attrs: Attrs,
}

impl Drop for LTNode {
    /// Drops deep trees (text groups nest as deep as there are text boxes) without recursing.
    fn drop(&mut self) {
        fn take_children(attrs: &mut Attrs, stack: &mut Vec<LTItem>) {
            if let Attrs::Container(c) = attrs {
                stack.append(c._objs.get_mut());
                if let Some(groups) = c.groups.get_mut().take() {
                    stack.extend(groups);
                }
            }
        }
        let mut stack = Vec::new();
        take_children(&mut self.attrs, &mut stack);
        while let Some(item) = stack.pop() {
            if let Ok(mut node) = Rc::try_unwrap(item.0) {
                take_children(&mut node.attrs, &mut stack);
            }
        }
    }
}

thread_local! {
    static NEXT_SERIAL: Cell<u64> = const { Cell::new(0) };
}

/// A reference to a layout object (a Python object reference: clones share the object).
#[derive(Clone)]
pub struct LTItem(Rc<LTNode>);

impl PartialEq for LTItem {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for LTItem {}

impl Hash for LTItem {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.serial.hash(state);
    }
}

impl fmt::Debug for LTItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.repr())
    }
}

const NO_BBOX: Geom = Geom { x0: 0.0, y0: 0.0, x1: 0.0, y1: 0.0, width: 0.0, height: 0.0 };

impl LTItem {
    fn new(class: LTClass, geom: Geom, attrs: Attrs) -> LTItem {
        let serial = NEXT_SERIAL.with(|s| {
            let v = s.get();
            s.set(v + 1);
            v
        });
        LTItem(Rc::new(LTNode { serial, class, geom: Cell::new(geom), attrs }))
    }

    fn new_container(class: LTClass, bbox: Rect, c: ContainerAttrs) -> LTItem {
        LTItem::new(class, Geom::from_bbox(bbox), Attrs::Container(Box::new(c)))
    }

    /// `id(obj)` as the port defines it: the creation order (DIVERGE(heap-id-ties)).
    pub fn id(&self) -> u64 {
        self.0.serial
    }

    /// `self is other`.
    pub fn is(&self, other: &LTItem) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    /// `type(self)`.
    pub fn class(&self) -> LTClass {
        self.0.class
    }

    /// `type(self).__name__`.
    pub fn class_name(&self) -> &'static str {
        self.0.class.name()
    }

    /// `isinstance(self, cls)`.
    pub fn isinstance(&self, cls: LTClass) -> bool {
        self.0.class.is_subclass_of(cls)
    }

    pub fn is_ltitem(&self) -> bool {
        true
    }
    pub fn is_lttext(&self) -> bool {
        self.0.class.is_text()
    }
    pub fn is_ltcomponent(&self) -> bool {
        self.isinstance(LTClass::LTComponent)
    }
    pub fn is_ltcurve(&self) -> bool {
        self.isinstance(LTClass::LTCurve)
    }
    pub fn is_ltline(&self) -> bool {
        self.isinstance(LTClass::LTLine)
    }
    pub fn is_ltrect(&self) -> bool {
        self.isinstance(LTClass::LTRect)
    }
    pub fn is_ltimage(&self) -> bool {
        self.isinstance(LTClass::LTImage)
    }
    pub fn is_ltanno(&self) -> bool {
        self.isinstance(LTClass::LTAnno)
    }
    pub fn is_ltchar(&self) -> bool {
        self.isinstance(LTClass::LTChar)
    }
    pub fn is_ltcontainer(&self) -> bool {
        self.isinstance(LTClass::LTContainer)
    }
    pub fn is_ltexpandablecontainer(&self) -> bool {
        self.isinstance(LTClass::LTExpandableContainer)
    }
    pub fn is_lttextcontainer(&self) -> bool {
        self.isinstance(LTClass::LTTextContainer)
    }
    pub fn is_lttextline(&self) -> bool {
        self.isinstance(LTClass::LTTextLine)
    }
    pub fn is_lttextlinehorizontal(&self) -> bool {
        self.isinstance(LTClass::LTTextLineHorizontal)
    }
    pub fn is_lttextlinevertical(&self) -> bool {
        self.isinstance(LTClass::LTTextLineVertical)
    }
    pub fn is_lttextbox(&self) -> bool {
        self.isinstance(LTClass::LTTextBox)
    }
    pub fn is_lttextboxhorizontal(&self) -> bool {
        self.isinstance(LTClass::LTTextBoxHorizontal)
    }
    pub fn is_lttextboxvertical(&self) -> bool {
        self.isinstance(LTClass::LTTextBoxVertical)
    }
    pub fn is_lttextgroup(&self) -> bool {
        self.isinstance(LTClass::LTTextGroup)
    }
    pub fn is_lttextgrouplrtb(&self) -> bool {
        self.isinstance(LTClass::LTTextGroupLRTB)
    }
    pub fn is_lttextgrouptbrl(&self) -> bool {
        self.isinstance(LTClass::LTTextGroupTBRL)
    }
    pub fn is_ltlayoutcontainer(&self) -> bool {
        self.isinstance(LTClass::LTLayoutContainer)
    }
    pub fn is_ltfigure(&self) -> bool {
        self.isinstance(LTClass::LTFigure)
    }
    pub fn is_ltpage(&self) -> bool {
        self.isinstance(LTClass::LTPage)
    }

    // --- per-class attributes

    /// The attributes of an `LTChar`.
    pub fn as_char(&self) -> Option<&LTCharAttrs> {
        match &self.0.attrs {
            Attrs::Char(c) => Some(c),
            _ => None,
        }
    }

    /// The attributes of an `LTCurve`, `LTLine` or `LTRect`.
    pub fn as_curve(&self) -> Option<&LTCurveAttrs> {
        match &self.0.attrs {
            Attrs::Curve(c) => Some(c),
            _ => None,
        }
    }

    /// The attributes of an `LTImage`.
    pub fn as_image(&self) -> Option<&LTImageAttrs> {
        match &self.0.attrs {
            Attrs::Image(c) => Some(c),
            _ => None,
        }
    }

    /// The attributes of an `LTFigure`.
    pub fn as_figure(&self) -> Option<&LTFigureAttrs> {
        self.container_opt().and_then(|c| c.figure.as_ref())
    }

    /// The attributes of an `LTPage`.
    pub fn as_page(&self) -> Option<&LTPageAttrs> {
        self.container_opt().and_then(|c| c.page.as_ref())
    }

    fn container_opt(&self) -> Option<&ContainerAttrs> {
        match &self.0.attrs {
            Attrs::Container(c) => Some(c),
            _ => None,
        }
    }

    fn container(&self) -> &ContainerAttrs {
        self.container_opt().unwrap_or_else(|| panic!("{} is not a container", self.class_name()))
    }

    // --- LTComponent

    pub fn x0(&self) -> f64 {
        self.0.geom.get().x0
    }
    pub fn y0(&self) -> f64 {
        self.0.geom.get().y0
    }
    pub fn x1(&self) -> f64 {
        self.0.geom.get().x1
    }
    pub fn y1(&self) -> f64 {
        self.0.geom.get().y1
    }
    pub fn width(&self) -> f64 {
        self.0.geom.get().width
    }
    pub fn height(&self) -> f64 {
        self.0.geom.get().height
    }
    /// `self.bbox`.
    pub fn bbox(&self) -> Rect {
        let g = self.0.geom.get();
        (g.x0, g.y0, g.x1, g.y1)
    }

    /// `set_bbox(bbox)`.
    pub fn set_bbox(&self, bbox: Rect) {
        self.0.geom.set(Geom::from_bbox(bbox));
    }

    /// `is_empty()` (`LTTextLine` also counts whitespace-only lines as empty).
    pub fn is_empty(&self) -> bool {
        let base = self.width() <= 0.0 || self.height() <= 0.0;
        if self.is_lttextline() {
            return base || self.get_text().isspace();
        }
        base
    }

    pub fn is_hoverlap(&self, obj: &LTItem) -> bool {
        debug_assert!(obj.is_ltcomponent(), "{}", obj.class_name());
        obj.x0() <= self.x1() && self.x0() <= obj.x1()
    }

    pub fn hdistance(&self, obj: &LTItem) -> f64 {
        debug_assert!(obj.is_ltcomponent(), "{}", obj.class_name());
        if self.is_hoverlap(obj) { 0.0 } else { pymin((self.x0() - obj.x1()).abs(), (self.x1() - obj.x0()).abs()) }
    }

    pub fn hoverlap(&self, obj: &LTItem) -> f64 {
        debug_assert!(obj.is_ltcomponent(), "{}", obj.class_name());
        if self.is_hoverlap(obj) { pymin((self.x0() - obj.x1()).abs(), (self.x1() - obj.x0()).abs()) } else { 0.0 }
    }

    pub fn is_voverlap(&self, obj: &LTItem) -> bool {
        debug_assert!(obj.is_ltcomponent(), "{}", obj.class_name());
        obj.y0() <= self.y1() && self.y0() <= obj.y1()
    }

    pub fn vdistance(&self, obj: &LTItem) -> f64 {
        debug_assert!(obj.is_ltcomponent(), "{}", obj.class_name());
        if self.is_voverlap(obj) { 0.0 } else { pymin((self.y0() - obj.y1()).abs(), (self.y1() - obj.y0()).abs()) }
    }

    pub fn voverlap(&self, obj: &LTItem) -> f64 {
        debug_assert!(obj.is_ltcomponent(), "{}", obj.class_name());
        if self.is_voverlap(obj) { pymin((self.y0() - obj.y1()).abs(), (self.y1() - obj.y0()).abs()) } else { 0.0 }
    }

    /// `self < other`: comparison is disabled for components (`PDFValueError`).
    pub fn __lt__(&self, _other: &LTItem) -> PyResult<bool> {
        Err(PyErr::new(ExcKind::PDFValueError, ""))
    }
    pub fn __le__(&self, _other: &LTItem) -> PyResult<bool> {
        Err(PyErr::new(ExcKind::PDFValueError, ""))
    }
    pub fn __gt__(&self, _other: &LTItem) -> PyResult<bool> {
        Err(PyErr::new(ExcKind::PDFValueError, ""))
    }
    pub fn __ge__(&self, _other: &LTItem) -> PyResult<bool> {
        Err(PyErr::new(ExcKind::PDFValueError, ""))
    }

    // --- LTText

    /// `get_text()` of an `LTText` (`LTChar`, `LTAnno`, text containers). Other objects have no
    /// `get_text` in Python; calling it on them is a bug in the caller.
    pub fn get_text(&self) -> PyStr {
        match &self.0.attrs {
            Attrs::Anno(t) => t.clone(),
            Attrs::Char(c) => c._text.clone(),
            Attrs::Container(c) if self.is_lttextcontainer() => {
                let mut out = PyStr::new();
                for obj in c._objs.borrow().iter() {
                    if obj.is_lttext() {
                        out.push_pystr(&obj.get_text());
                    }
                }
                out
            }
            _ => panic!("AttributeError: '{}' object has no attribute 'get_text'", self.class_name()),
        }
    }

    // --- LTContainer

    /// `list(self)`: the children.
    pub fn objs(&self) -> Vec<LTItem> {
        self.container()._objs.borrow().clone()
    }

    /// The children without copying the list (do not mutate the container meanwhile).
    pub fn objs_ref(&self) -> Ref<'_, Vec<LTItem>> {
        self.container()._objs.borrow()
    }

    /// `len(self)`.
    pub fn len(&self) -> usize {
        self.container()._objs.borrow().len()
    }

    /// `len(self) == 0`.
    pub fn is_empty_container(&self) -> bool {
        self.container()._objs.borrow().is_empty()
    }

    /// `LTContainer.add(self, obj)`.
    fn container_add(&self, obj: LTItem) {
        self.container()._objs.borrow_mut().push(obj);
    }

    /// `LTExpandableContainer.add(self, obj)`.
    fn expandable_add(&self, obj: LTItem) {
        let (ox0, oy0, ox1, oy1) = obj.bbox();
        self.container_add(obj);
        self.set_bbox((pymin(self.x0(), ox0), pymin(self.y0(), oy0), pymax(self.x1(), ox1), pymax(self.y1(), oy1)));
    }

    /// `add(obj)`, as the object's class defines it.
    pub fn add(&self, obj: LTItem) {
        use LTClass::*;
        match self.class() {
            LTTextLineHorizontal => {
                let c = self.container();
                if obj.is_ltchar() && c.word_margin != 0.0 {
                    let margin = c.word_margin * pymax(obj.width(), obj.height());
                    if c.edge.get() < obj.x0() - margin {
                        self.container_add(self::LTAnno(PyStr::from(" ")));
                    }
                }
                c.edge.set(obj.x1());
                self.expandable_add(obj);
            }
            LTTextLineVertical => {
                let c = self.container();
                if obj.is_ltchar() && c.word_margin != 0.0 {
                    let margin = c.word_margin * pymax(obj.width(), obj.height());
                    if obj.y1() + margin < c.edge.get() {
                        self.container_add(self::LTAnno(PyStr::from(" ")));
                    }
                }
                c.edge.set(obj.y0());
                self.expandable_add(obj);
            }
            LTExpandableContainer
            | LTTextContainer
            | LTTextLine
            | LTTextBox
            | LTTextBoxHorizontal
            | LTTextBoxVertical
            | LTTextGroup
            | LTTextGroupLRTB
            | LTTextGroupTBRL => self.expandable_add(obj),
            LTContainer | LTLayoutContainer | LTFigure | LTPage => self.container_add(obj),
            _ => panic!("AttributeError: '{}' object has no attribute 'add'", self.class_name()),
        }
    }

    /// `extend(objs)`.
    pub fn extend(&self, objs: impl IntoIterator<Item = LTItem>) {
        for obj in objs {
            self.add(obj);
        }
    }

    // --- LTTextLine, LTTextBox, LTLayoutContainer attributes

    /// `LTTextLine.word_margin`.
    pub fn word_margin(&self) -> f64 {
        self.container().word_margin
    }

    /// `LTTextBox.index`.
    pub fn index(&self) -> i64 {
        self.container().index.get()
    }

    pub fn set_index(&self, index: i64) {
        self.container().index.set(index);
    }

    /// `LTTextBox.get_writing_mode()`.
    pub fn get_writing_mode(&self) -> PyResult<&'static str> {
        match self.class() {
            LTClass::LTTextBoxHorizontal => Ok("lr-tb"),
            LTClass::LTTextBoxVertical => Ok("tb-rl"),
            _ => Err(PyErr::new(ExcKind::NotImplementedError, "")),
        }
    }

    /// `LTLayoutContainer.groups`.
    pub fn groups(&self) -> Option<Vec<LTItem>> {
        self.container().groups.borrow().clone()
    }

    pub fn set_groups(&self, groups: Option<Vec<LTItem>>) {
        *self.container().groups.borrow_mut() = groups;
    }

    // --- repr

    /// `repr(self)`.
    pub fn repr(&self) -> String {
        use LTClass::*;
        let name = self.class_name();
        match self.class() {
            LTAnno => format!("<{} {}>", name, pystr_repr(&self.get_text())),
            LTChar => {
                let c = self.as_char().expect("LTChar attrs");
                format!(
                    "<{} {} matrix={} font={} adv={} text={}>",
                    name,
                    bbox2str(self.bbox()),
                    matrix2str(c.matrix),
                    c.fontname.repr(),
                    float_repr(c.adv),
                    pystr_repr(&self.get_text())
                )
            }
            LTImage => {
                let i = self.as_image().expect("LTImage attrs");
                format!(
                    "<{}({}) {} ({}, {})>",
                    name,
                    i.name,
                    bbox2str(self.bbox()),
                    i.srcsize.0.repr(),
                    i.srcsize.1.repr()
                )
            }
            LTTextLine | LTTextLineHorizontal | LTTextLineVertical => {
                format!("<{} {} {}>", name, bbox2str(self.bbox()), pystr_repr(&self.get_text()))
            }
            LTTextBox | LTTextBoxHorizontal | LTTextBoxVertical => {
                format!("<{}({}) {} {}>", name, self.index(), bbox2str(self.bbox()), pystr_repr(&self.get_text()))
            }
            LTFigure => {
                let f = self.as_figure().expect("LTFigure attrs");
                format!("<{}({}) {} matrix={}>", name, f.name, bbox2str(self.bbox()), matrix2str(f.matrix))
            }
            LTPage => {
                let p = self.as_page().expect("LTPage attrs");
                format!("<{}({}) {} rotate={}>", name, p.pageid, bbox2str(self.bbox()), p.rotate.repr())
            }
            _ => format!("<{} {}>", name, bbox2str(self.bbox())),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Constructors (Python's class calls)

/// `LTComponent(bbox)`.
pub fn LTComponent(bbox: Rect) -> LTItem {
    LTItem::new(LTClass::LTComponent, Geom::from_bbox(bbox), Attrs::None)
}

/// `LTCurve(linewidth, pts, stroke, fill, evenodd, stroking_color, non_stroking_color,
/// original_path, dashing_style)`.
pub fn LTCurve(
    linewidth: Value,
    pts: Vec<Point>,
    stroke: bool,
    fill: bool,
    evenodd: bool,
    stroking_color: Option<Color>,
    non_stroking_color: Option<Color>,
    original_path: Option<Vec<PathSegment>>,
    dashing_style: Option<(Value, Value)>,
) -> LTItem {
    new_curve(
        LTClass::LTCurve,
        linewidth,
        pts,
        stroke,
        fill,
        evenodd,
        stroking_color,
        non_stroking_color,
        original_path,
        dashing_style,
    )
}

fn new_curve(
    class: LTClass,
    linewidth: Value,
    pts: Vec<Point>,
    stroke: bool,
    fill: bool,
    evenodd: bool,
    stroking_color: Option<Color>,
    non_stroking_color: Option<Color>,
    original_path: Option<Vec<PathSegment>>,
    dashing_style: Option<(Value, Value)>,
) -> LTItem {
    let geom = Geom::from_bbox(get_bound(pts.iter().copied()));
    let attrs = LTCurveAttrs {
        pts,
        linewidth,
        stroke,
        fill,
        evenodd,
        stroking_color,
        non_stroking_color,
        original_path,
        dashing_style,
    };
    LTItem::new(class, geom, Attrs::Curve(Box::new(attrs)))
}

/// `LTLine(linewidth, p0, p1, stroke, fill, evenodd, stroking_color, non_stroking_color,
/// original_path, dashing_style)`: a single straight line.
pub fn LTLine(
    linewidth: Value,
    p0: Point,
    p1: Point,
    stroke: bool,
    fill: bool,
    evenodd: bool,
    stroking_color: Option<Color>,
    non_stroking_color: Option<Color>,
    original_path: Option<Vec<PathSegment>>,
    dashing_style: Option<(Value, Value)>,
) -> LTItem {
    new_curve(
        LTClass::LTLine,
        linewidth,
        vec![p0, p1],
        stroke,
        fill,
        evenodd,
        stroking_color,
        non_stroking_color,
        original_path,
        dashing_style,
    )
}

/// `LTRect(linewidth, bbox, stroke, fill, evenodd, stroking_color, non_stroking_color,
/// original_path, dashing_style)`: a rectangle.
pub fn LTRect(
    linewidth: Value,
    bbox: Rect,
    stroke: bool,
    fill: bool,
    evenodd: bool,
    stroking_color: Option<Color>,
    non_stroking_color: Option<Color>,
    original_path: Option<Vec<PathSegment>>,
    dashing_style: Option<(Value, Value)>,
) -> LTItem {
    let (x0, y0, x1, y1) = bbox;
    new_curve(
        LTClass::LTRect,
        linewidth,
        vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)],
        stroke,
        fill,
        evenodd,
        stroking_color,
        non_stroking_color,
        original_path,
        dashing_style,
    )
}

/// `LTImage(name, stream, bbox)`: an image object.
pub fn LTImage(name: &str, stream: Rc<PDFStream>, bbox: Rect) -> LTItem {
    let srcsize = (stream.get_any(&["W", "Width"], Value::None), stream.get_any(&["H", "Height"], Value::None));
    let imagemask = stream.get_any(&["IM", "ImageMask"], Value::None);
    let bits = stream.get_any(&["BPC", "BitsPerComponent"], Value::Int(1));
    let mut colorspace = stream.get_any(&["CS", "ColorSpace"], Value::None);
    if !matches!(colorspace, Value::List(_)) {
        colorspace = Value::list(vec![colorspace]);
    }
    let attrs = LTImageAttrs { name: name.to_string(), stream, srcsize, imagemask, bits, colorspace };
    LTItem::new(LTClass::LTImage, Geom::from_bbox(bbox), Attrs::Image(Box::new(attrs)))
}

/// `LTAnno(text)`: a virtual character inserted by the layout analysis (no bbox).
pub fn LTAnno(text: PyStr) -> LTItem {
    LTItem::new(LTClass::LTAnno, NO_BBOX, Attrs::Anno(text))
}

/// `LTChar(matrix, font, fontsize, scaling, rise, text, textwidth, textdisp, ncs,
/// graphicstate)`. `textdisp` is `font.char_disp(cid)` as Python has it (a number, or a tuple
/// `(vx or None, vy)` for vertical fonts).
pub fn LTChar(
    matrix: Matrix,
    font: &PDFFont,
    fontsize: f64,
    scaling: f64,
    rise: f64,
    text: PyStr,
    textwidth: f64,
    textdisp: &Value,
    ncs: PDFColorSpace,
    graphicstate: PDFGraphicStateRef,
) -> PyResult<LTItem> {
    new_char(
        matrix,
        font.fontname().clone(),
        font.is_vertical(),
        &|| font.get_descent(),
        fontsize,
        scaling,
        rise,
        text,
        textwidth,
        textdisp,
        ncs,
        graphicstate,
    )
}

/// `LTChar(...)` with the font's answers given directly (`font.fontname`,
/// `font.is_vertical()`, `font.get_descent()`): for tests that have no `PDFFont`.
#[doc(hidden)]
pub fn LTChar_from_font_values(
    matrix: Matrix,
    fontname: Value,
    vertical: bool,
    descent: f64,
    fontsize: f64,
    scaling: f64,
    rise: f64,
    text: PyStr,
    textwidth: f64,
    textdisp: &Value,
    ncs: PDFColorSpace,
    graphicstate: PDFGraphicStateRef,
) -> PyResult<LTItem> {
    new_char(
        matrix,
        fontname,
        vertical,
        &|| Ok(descent),
        fontsize,
        scaling,
        rise,
        text,
        textwidth,
        textdisp,
        ncs,
        graphicstate,
    )
}

/// An `LTChar` with its final attributes given directly: for tests.
#[doc(hidden)]
pub fn LTChar_from_attrs(
    text: PyStr,
    matrix: Matrix,
    fontname: Value,
    adv: f64,
    upright: bool,
    size: f64,
    bbox: Rect,
    ncs: PDFColorSpace,
    graphicstate: PDFGraphicStateRef,
) -> LTItem {
    let attrs = LTCharAttrs { _text: text, matrix, fontname, ncs, graphicstate, adv, upright, size };
    LTItem::new(LTClass::LTChar, Geom::from_bbox(bbox), Attrs::Char(attrs))
}

/// `vx * fontsize` and `... * 0.001` in `LTChar.__init__`: Python arithmetic on the untyped
/// displacement (a sequence times a float raises Python's own message).
fn mul_float(a: &Value, b: f64) -> PyResult<Value> {
    match a {
        Value::Bytes(_) | Value::Str(_) | Value::List(_) | Value::Tuple(_) => {
            Err(PyErr::new(ExcKind::TypeError, "can't multiply sequence by non-int of type 'float'"))
        }
        _ => py_mul(a, &Value::Float(b)),
    }
}

fn float_of(v: Value) -> f64 {
    v.as_f64().expect("a number times a float is a float")
}

fn new_char(
    matrix: Matrix,
    fontname: Value,
    vertical: bool,
    get_descent: &dyn Fn() -> PyResult<f64>,
    fontsize: f64,
    scaling: f64,
    rise: f64,
    text: PyStr,
    textwidth: f64,
    textdisp: &Value,
    ncs: PDFColorSpace,
    graphicstate: PDFGraphicStateRef,
) -> PyResult<LTItem> {
    let adv = textwidth * fontsize * scaling;
    // compute the boundary rectangle.
    let bbox: Rect = if vertical {
        // vertical
        let Value::Tuple(t) = textdisp else {
            return Err(PyErr::new(ExcKind::AssertionError, ""));
        };
        if t.len() != 2 {
            let msg = if t.len() > 2 {
                format!("too many values to unpack (expected 2, got {})", t.len())
            } else {
                format!("not enough values to unpack (expected 2, got {})", t.len())
            };
            return Err(PyErr::new(ExcKind::ValueError, msg));
        }
        let (vx, vy) = (&t[0], &t[1]);
        let vx = if vx.is_none() { fontsize * 0.5 } else { float_of(mul_float(&mul_float(vx, fontsize)?, 0.001)?) };
        let vy = float_of(mul_float(&mul_float(&py_sub(&Value::Int(1000), vy)?, fontsize)?, 0.001)?);
        (-vx, vy + rise + adv, -vx + fontsize, vy + rise)
    } else {
        // horizontal
        let descent = get_descent()? * fontsize;
        (0.0, descent + rise, adv, descent + rise + fontsize)
    };
    let (a, b, c, d, _e, _f) = matrix;
    let upright = a * d * scaling > 0.0 && b * c <= 0.0;
    let (mut x0, mut y0, mut x1, mut y1) = apply_matrix_rect(matrix, bbox);
    if x1 < x0 {
        (x0, x1) = (x1, x0);
    }
    if y1 < y0 {
        (y0, y1) = (y1, y0);
    }
    let geom = Geom::from_bbox((x0, y0, x1, y1));
    let size = if vertical { geom.width } else { geom.height };
    let attrs = LTCharAttrs { _text: text, matrix, fontname, ncs, graphicstate, adv, upright, size };
    Ok(LTItem::new(LTClass::LTChar, geom, Attrs::Char(attrs)))
}

/// `LTContainer(bbox)`.
pub fn LTContainer(bbox: Rect) -> LTItem {
    LTItem::new_container(LTClass::LTContainer, bbox, ContainerAttrs::new())
}

const EXPANDABLE_BBOX: Rect = (INF as f64, INF as f64, -INF as f64, -INF as f64);

fn new_expandable(class: LTClass, c: ContainerAttrs) -> LTItem {
    LTItem::new_container(class, EXPANDABLE_BBOX, c)
}

/// `LTExpandableContainer()`.
pub fn LTExpandableContainer() -> LTItem {
    new_expandable(LTClass::LTExpandableContainer, ContainerAttrs::new())
}

/// `LTTextContainer()`.
pub fn LTTextContainer() -> LTItem {
    new_expandable(LTClass::LTTextContainer, ContainerAttrs::new())
}

fn new_textline(class: LTClass, word_margin: f64, edge: f64) -> LTItem {
    let mut c = ContainerAttrs::new();
    c.word_margin = word_margin;
    c.edge = Cell::new(edge);
    new_expandable(class, c)
}

/// `LTTextLine(word_margin)`.
pub fn LTTextLine(word_margin: f64) -> LTItem {
    new_textline(LTClass::LTTextLine, word_margin, 0.0)
}

/// `LTTextLineHorizontal(word_margin)`.
pub fn LTTextLineHorizontal(word_margin: f64) -> LTItem {
    new_textline(LTClass::LTTextLineHorizontal, word_margin, INF as f64)
}

/// `LTTextLineVertical(word_margin)`.
pub fn LTTextLineVertical(word_margin: f64) -> LTItem {
    new_textline(LTClass::LTTextLineVertical, word_margin, -INF as f64)
}

/// `LTTextBox()`.
pub fn LTTextBox() -> LTItem {
    new_expandable(LTClass::LTTextBox, ContainerAttrs::new())
}

/// `LTTextBoxHorizontal()`.
pub fn LTTextBoxHorizontal() -> LTItem {
    new_expandable(LTClass::LTTextBoxHorizontal, ContainerAttrs::new())
}

/// `LTTextBoxVertical()`.
pub fn LTTextBoxVertical() -> LTItem {
    new_expandable(LTClass::LTTextBoxVertical, ContainerAttrs::new())
}

fn new_textgroup(class: LTClass, objs: impl IntoIterator<Item = LTItem>) -> LTItem {
    let g = new_expandable(class, ContainerAttrs::new());
    g.extend(objs);
    g
}

/// `LTTextGroup(objs)`.
pub fn LTTextGroup(objs: impl IntoIterator<Item = LTItem>) -> LTItem {
    new_textgroup(LTClass::LTTextGroup, objs)
}

/// `LTTextGroupLRTB(objs)`.
pub fn LTTextGroupLRTB(objs: impl IntoIterator<Item = LTItem>) -> LTItem {
    new_textgroup(LTClass::LTTextGroupLRTB, objs)
}

/// `LTTextGroupTBRL(objs)`.
pub fn LTTextGroupTBRL(objs: impl IntoIterator<Item = LTItem>) -> LTItem {
    new_textgroup(LTClass::LTTextGroupTBRL, objs)
}

/// `LTLayoutContainer(bbox)`.
pub fn LTLayoutContainer(bbox: Rect) -> LTItem {
    LTItem::new_container(LTClass::LTLayoutContainer, bbox, ContainerAttrs::new())
}

/// `LTFigure(name, bbox, matrix)`: `bbox` is `(x, y, width, height)` in the form's space.
pub fn LTFigure(name: &str, bbox: Rect, matrix: Matrix) -> LTItem {
    let (x, y, w, h) = bbox;
    let rect = (x, y, x + w, y + h);
    let bbox = apply_matrix_rect(matrix, rect);
    let mut c = ContainerAttrs::new();
    c.figure = Some(LTFigureAttrs { name: name.to_string(), matrix });
    LTItem::new_container(LTClass::LTFigure, bbox, c)
}

/// `LTPage(pageid, bbox, rotate)` (`rotate` defaults to the int `0` in Python).
pub fn LTPage(pageid: i64, bbox: Rect, rotate: Value) -> LTItem {
    let mut c = ContainerAttrs::new();
    c.page = Some(LTPageAttrs { pageid, rotate });
    LTItem::new_container(LTClass::LTPage, bbox, c)
}

// ---------------------------------------------------------------------------------------------
// Analysis

/// An entry of `group_textboxes`' heap: `(skip_isany, d, id(obj1), id(obj2), obj1, obj2)`, the
/// objects given as indices into the function's list of objects.
struct DistEntry {
    skip_isany: bool,
    d: f64,
    id1: u64,
    id2: u64,
    obj1: usize,
    obj2: usize,
}

/// Python's tuple `<` on two heap entries: the first items that differ (`==` false; a NaN
/// distance never equals another entry's) decide with `<`.
fn dist_entry_lt(a: &DistEntry, b: &DistEntry) -> bool {
    if a.skip_isany != b.skip_isany {
        return !a.skip_isany & b.skip_isany;
    }
    #[allow(clippy::float_cmp)]
    if a.d != b.d {
        return a.d < b.d;
    }
    if a.id1 != b.id1 {
        return a.id1 < b.id1;
    }
    if a.id2 != b.id2 {
        return a.id2 < b.id2;
    }
    // Same ids: the same objects, so the tuples are equal.
    false
}

/// Python's `<` on the `(int, float, float)` keys of `analyze`'s `getkey`.
fn key3_lt(a: &(i64, f64, f64), b: &(i64, f64, f64)) -> bool {
    if a.0 != b.0 {
        return a.0 < b.0;
    }
    if a.1 != b.1 {
        return a.1 < b.1;
    }
    if a.2 != b.2 {
        return a.2 < b.2;
    }
    false
}

impl LTItem {
    /// `analyze(laparams)`: perform the layout analysis (as called from a converter: the
    /// object's frame at [`ANALYZE_FRAME_DEPTH`]).
    pub fn analyze(&self, laparams: &LAParams) -> PyResult<()> {
        self.analyze_at(laparams, ANALYZE_FRAME_DEPTH)
    }

    /// `analyze(laparams)` with its Python frame at `depth`.
    pub fn analyze_at(&self, laparams: &LAParams, depth: usize) -> PyResult<()> {
        use LTClass::*;
        enter_frame(depth)?;
        match self.class() {
            LTComponent | LTCurve | LTLine | LTRect | LTImage | LTAnno | LTChar => Ok(()),
            LTContainer | LTExpandableContainer | LTTextContainer | LTTextBox | LTTextGroup => {
                self.container_analyze(laparams, depth)
            }
            LTTextLine | LTTextLineHorizontal | LTTextLineVertical => {
                for obj in self.objs() {
                    obj.analyze_at(laparams, depth + 1)?;
                }
                // `LTContainer.add(self, LTAnno("\n"))`
                enter_frame(depth + 1)?;
                self.container_add(self::LTAnno(PyStr::from("\n")));
                Ok(())
            }
            LTTextBoxHorizontal => {
                self.container_analyze(laparams, depth + 1)?;
                sort_by_f64_key(&mut self.container()._objs.borrow_mut(), |obj| -obj.y1());
                Ok(())
            }
            LTTextBoxVertical => {
                self.container_analyze(laparams, depth + 1)?;
                sort_by_f64_key(&mut self.container()._objs.borrow_mut(), |obj| -obj.x1());
                Ok(())
            }
            LTTextGroupLRTB => {
                self.container_analyze(laparams, depth + 1)?;
                let Some(boxes_flow) = laparams.boxes_flow else {
                    return Err(PyErr::new(ExcKind::AssertionError, ""));
                };
                // reorder the objects from top-left to bottom-right.
                sort_by_f64_key(&mut self.container()._objs.borrow_mut(), |obj| {
                    (1.0 - boxes_flow) * obj.x0() - (1.0 + boxes_flow) * (obj.y0() + obj.y1())
                });
                Ok(())
            }
            LTTextGroupTBRL => {
                self.container_analyze(laparams, depth + 1)?;
                let Some(boxes_flow) = laparams.boxes_flow else {
                    return Err(PyErr::new(ExcKind::AssertionError, ""));
                };
                // reorder the objects from top-right to bottom-left.
                sort_by_f64_key(&mut self.container()._objs.borrow_mut(), |obj| {
                    -(1.0 + boxes_flow) * (obj.x0() + obj.x1()) - (1.0 - boxes_flow) * obj.y1()
                });
                Ok(())
            }
            LTLayoutContainer | LTPage => self.layout_analyze(laparams, depth),
            LTFigure => {
                if !laparams.all_texts {
                    return Ok(());
                }
                self.layout_analyze(laparams, depth + 1)
            }
        }
    }

    /// `LTContainer.analyze(self, laparams)`, its frame at `depth`.
    fn container_analyze(&self, laparams: &LAParams, depth: usize) -> PyResult<()> {
        enter_frame(depth)?;
        for obj in self.objs() {
            obj.analyze_at(laparams, depth + 1)?;
        }
        Ok(())
    }

    /// `LTTextLine.find_neighbors(plane, ratio)`.
    ///
    /// Finds neighboring lines of the same orientation in the plane: the other lines close to
    /// self ("close" is controlled by ratio) that have the same height (width for vertical
    /// lines) and are left-, right- or centrally-aligned (lower-, upper- or centrally-aligned).
    pub fn find_neighbors(&self, plane: &Plane, ratio: f64) -> PyResult<Vec<LTItem>> {
        match self.class() {
            LTClass::LTTextLineHorizontal => {
                let d = ratio * self.height();
                let objs = plane.find((self.x0(), self.y0() - d, self.x1(), self.y1() + d))?;
                Ok(objs
                    .into_iter()
                    .filter(|obj| {
                        obj.is_lttextlinehorizontal()
                            && self._is_same_height_as(obj, d)
                            && (self._is_left_aligned_with(obj, d)
                                || self._is_right_aligned_with(obj, d)
                                || self._is_centrally_aligned_with_h(obj, d))
                    })
                    .collect())
            }
            LTClass::LTTextLineVertical => {
                let d = ratio * self.width();
                let objs = plane.find((self.x0() - d, self.y0(), self.x1() + d, self.y1()))?;
                Ok(objs
                    .into_iter()
                    .filter(|obj| {
                        obj.is_lttextlinevertical()
                            && self._is_same_width_as(obj, d)
                            && (self._is_lower_aligned_with(obj, d)
                                || self._is_upper_aligned_with(obj, d)
                                || self._is_centrally_aligned_with_v(obj, d))
                    })
                    .collect())
            }
            _ => Err(PyErr::new(ExcKind::NotImplementedError, "")),
        }
    }

    /// Whether the left-hand edge of `other` is within `tolerance`.
    pub fn _is_left_aligned_with(&self, other: &LTItem, tolerance: f64) -> bool {
        (other.x0() - self.x0()).abs() <= tolerance
    }

    /// Whether the right-hand edge of `other` is within `tolerance`.
    pub fn _is_right_aligned_with(&self, other: &LTItem, tolerance: f64) -> bool {
        (other.x1() - self.x1()).abs() <= tolerance
    }

    /// `LTTextLineHorizontal._is_centrally_aligned_with`: whether the horizontal center of
    /// `other` is within `tolerance`.
    pub fn _is_centrally_aligned_with_h(&self, other: &LTItem, tolerance: f64) -> bool {
        ((other.x0() + other.x1()) / 2.0 - (self.x0() + self.x1()) / 2.0).abs() <= tolerance
    }

    pub fn _is_same_height_as(&self, other: &LTItem, tolerance: f64) -> bool {
        (other.height() - self.height()).abs() <= tolerance
    }

    /// Whether the lower edge of `other` is within `tolerance`.
    pub fn _is_lower_aligned_with(&self, other: &LTItem, tolerance: f64) -> bool {
        (other.y0() - self.y0()).abs() <= tolerance
    }

    /// Whether the upper edge of `other` is within `tolerance`.
    pub fn _is_upper_aligned_with(&self, other: &LTItem, tolerance: f64) -> bool {
        (other.y1() - self.y1()).abs() <= tolerance
    }

    /// `LTTextLineVertical._is_centrally_aligned_with`: whether the vertical center of `other`
    /// is within `tolerance`.
    pub fn _is_centrally_aligned_with_v(&self, other: &LTItem, tolerance: f64) -> bool {
        ((other.y0() + other.y1()) / 2.0 - (self.y0() + self.y1()) / 2.0).abs() <= tolerance
    }

    pub fn _is_same_width_as(&self, other: &LTItem, tolerance: f64) -> bool {
        (other.width() - self.width()).abs() <= tolerance
    }

    /// `LTLayoutContainer.group_objects(laparams, objs)`: group text objects to textlines.
    pub fn group_objects(&self, laparams: &LAParams, objs: &[LTItem]) -> PyResult<Vec<LTItem>> {
        let mut out = Vec::new();
        let mut obj0: Option<&LTItem> = None;
        let mut line: Option<LTItem> = None;
        for obj1 in objs {
            if let Some(obj0) = obj0 {
                // halign: obj0 and obj1 is horizontally aligned.
                //
                //   +------+ - - -
                //   | obj0 | - - +------+   -
                //   |      |     | obj1 |   | (line_overlap)
                //   +------+ - - |      |   -
                //          - - - +------+
                //
                //          |<--->|
                //        (char_margin)
                let halign = obj0.is_voverlap(obj1)
                    && pymin(obj0.height(), obj1.height()) * laparams.line_overlap < obj0.voverlap(obj1)
                    && obj0.hdistance(obj1) < pymax(obj0.width(), obj1.width()) * laparams.char_margin;

                // valign: obj0 and obj1 is vertically aligned.
                //
                //   +------+
                //   | obj0 |
                //   |      |
                //   +------+ - - -
                //     |    |     | (char_margin)
                //     +------+ - -
                //     | obj1 |
                //     |      |
                //     +------+
                //
                //     |<-->|
                //   (line_overlap)
                let valign = laparams.detect_vertical
                    && obj0.is_hoverlap(obj1)
                    && pymin(obj0.width(), obj1.width()) * laparams.line_overlap < obj0.hoverlap(obj1)
                    && obj0.vdistance(obj1) < pymax(obj0.height(), obj1.height()) * laparams.char_margin;

                let line_h = line.as_ref().is_some_and(|l| l.is_lttextlinehorizontal());
                let line_v = line.as_ref().is_some_and(|l| l.is_lttextlinevertical());
                if (halign && line_h) || (valign && line_v) {
                    line.as_ref().expect("line").add(obj1.clone());
                } else if let Some(l) = line.take() {
                    out.push(l);
                } else if valign && !halign {
                    let l = LTTextLineVertical(laparams.word_margin);
                    l.add(obj0.clone());
                    l.add(obj1.clone());
                    line = Some(l);
                } else if halign && !valign {
                    let l = LTTextLineHorizontal(laparams.word_margin);
                    l.add(obj0.clone());
                    l.add(obj1.clone());
                    line = Some(l);
                } else {
                    let l = LTTextLineHorizontal(laparams.word_margin);
                    l.add(obj0.clone());
                    out.push(l);
                }
            }
            obj0 = Some(obj1);
        }
        let line = match line {
            Some(l) => l,
            None => {
                let l = LTTextLineHorizontal(laparams.word_margin);
                let Some(obj0) = obj0 else {
                    return Err(PyErr::new(ExcKind::AssertionError, ""));
                };
                l.add(obj0.clone());
                l
            }
        };
        out.push(line);
        Ok(out)
    }

    /// `LTLayoutContainer.group_textlines(laparams, lines)`: group neighboring lines to
    /// textboxes.
    #[allow(clippy::mutable_key_type)] // LTItem hashes by identity, which never changes
    pub fn group_textlines(&self, laparams: &LAParams, lines: &[LTItem]) -> PyResult<Vec<LTItem>> {
        let mut plane = Plane::new(self.bbox(), 50);
        plane.extend(lines.iter().cloned())?;
        let mut boxes: HashMap<LTItem, LTItem> = HashMap::new();
        for line in lines {
            let neighbors = line.find_neighbors(&plane, laparams.line_margin)?;
            let mut members = vec![line.clone()];
            for obj1 in neighbors {
                members.push(obj1.clone());
                if let Some(b) = boxes.remove(&obj1) {
                    members.extend(b.objs());
                }
            }
            let box_ = if line.is_lttextlinehorizontal() { LTTextBoxHorizontal() } else { LTTextBoxVertical() };
            for obj in uniq(members) {
                box_.add(obj.clone());
                boxes.insert(obj, box_.clone());
            }
        }
        let mut done: HashSet<LTItem> = HashSet::new();
        let mut out = Vec::new();
        for line in lines {
            let Some(box_) = boxes.get(line) else {
                continue;
            };
            if !done.insert(box_.clone()) {
                continue;
            }
            if !box_.is_empty() {
                out.push(box_.clone());
            }
        }
        Ok(out)
    }

    /// `LTLayoutContainer.group_textboxes(laparams, boxes)`: group textboxes hierarchically.
    ///
    /// Get pair-wise distances, via dist func defined below, and then merge from the closest
    /// textbox pair. Once obj1 and obj2 are merged / grouped, the resulting group is considered
    /// as a new object, and its distances to other objects & groups are added to the process
    /// queue.
    ///
    /// For performance reason, pair-wise distances and object pair info are maintained in a
    /// heap of (idx, dist, id(obj1), id(obj2), obj1, obj2) tuples. It ensures quick access to
    /// the smallest element. Note that since comparison operators, e.g., __lt__, are disabled
    /// for LTComponent, id(obj) has to appear before obj in element tuples.
    ///
    /// Returns a list that has only one element, the final top level group.
    pub fn group_textboxes(&self, _laparams: &LAParams, boxes: &[LTItem]) -> PyResult<Vec<LTItem>> {
        let mut plane = Plane::new(self.bbox(), 50);

        // A distance function between two TextBoxes.
        //
        // Consider the bounding rectangle for obj1 and obj2.
        // Return its area less the areas of obj1 and obj2,
        // shown as 'www' below. This value may be negative.
        //         +------+..........+ (x1, y1)
        //         | obj1 |wwwwwwwwww:
        //         +------+www+------+
        //         :wwwwwwwwww| obj2 |
        // (x0, y0) +..........+------+
        fn dist(obj1: &LTItem, obj2: &LTItem) -> f64 {
            let x0 = pymin(obj1.x0(), obj2.x0());
            let y0 = pymin(obj1.y0(), obj2.y0());
            let x1 = pymax(obj1.x1(), obj2.x1());
            let y1 = pymax(obj1.y1(), obj2.y1());
            (x1 - x0) * (y1 - y0) - obj1.width() * obj1.height() - obj2.width() * obj2.height()
        }

        // Check if there's any other object between obj1 and obj2.
        fn isany(plane: &Plane, obj1: &LTItem, obj2: &LTItem) -> PyResult<bool> {
            let x0 = pymin(obj1.x0(), obj2.x0());
            let y0 = pymin(obj1.y0(), obj2.y0());
            let x1 = pymax(obj1.x1(), obj2.x1());
            let y1 = pymax(obj1.y1(), obj2.y1());
            let objs = plane.find((x0, y0, x1, y1))?;
            Ok(objs.iter().any(|o| !o.is(obj1) && !o.is(obj2)))
        }

        // DIVERGE(heap-id-ties): `id(obj)` is the creation order (`LTItem::id`), not an address.
        // The heap holds indices into `elements` (the boxes, then the groups in creation order).
        let mut elements: Vec<LTItem> = boxes.to_vec();
        let mut element_index: HashMap<u64, usize> = boxes.iter().enumerate().map(|(i, b)| (b.id(), i)).collect();
        let mut dists: Vec<DistEntry> = Vec::with_capacity(boxes.len() * boxes.len().saturating_sub(1) / 2);
        for i in 0..boxes.len() {
            let box1 = &boxes[i];
            for (j, box2) in boxes.iter().enumerate().skip(i + 1) {
                dists.push(DistEntry {
                    skip_isany: false,
                    d: dist(box1, box2),
                    id1: box1.id(),
                    id2: box2.id(),
                    obj1: i,
                    obj2: j,
                });
            }
        }
        heapq::heapify(&mut dists, &dist_entry_lt);

        plane.extend(boxes.iter().cloned())?;
        let mut done: HashSet<u64> = HashSet::new();
        while let Some(entry) = heapq::heappop(&mut dists, &dist_entry_lt) {
            let DistEntry { skip_isany, d, id1, id2, obj1, obj2 } = entry;
            // Skip objects that are already merged
            if !done.contains(&id1) && !done.contains(&id2) {
                let (o1, o2) = (elements[obj1].clone(), elements[obj2].clone());
                if !skip_isany && isany(&plane, &o1, &o2)? {
                    heapq::heappush(
                        &mut dists,
                        DistEntry { skip_isany: true, d, id1, id2, obj1, obj2 },
                        &dist_entry_lt,
                    );
                    continue;
                }
                let group = if o1.is_lttextboxvertical()
                    || o1.is_lttextgrouptbrl()
                    || o2.is_lttextboxvertical()
                    || o2.is_lttextgrouptbrl()
                {
                    LTTextGroupTBRL([o1.clone(), o2.clone()])
                } else {
                    LTTextGroupLRTB([o1.clone(), o2.clone()])
                };
                plane.remove(&o1)?;
                plane.remove(&o2)?;
                done.insert(id1);
                done.insert(id2);

                let gi = elements.len();
                element_index.insert(group.id(), gi);
                elements.push(group.clone());
                for other in plane.iter() {
                    let entry = DistEntry {
                        skip_isany: false,
                        d: dist(&group, other),
                        id1: group.id(),
                        id2: other.id(),
                        obj1: gi,
                        obj2: element_index[&other.id()],
                    };
                    heapq::heappush(&mut dists, entry, &dist_entry_lt);
                }
                plane.add(group)?;
            }
        }
        // By now only groups are in the plane
        Ok(plane.iter().cloned().collect())
    }

    /// `LTLayoutContainer.analyze(self, laparams)`, its frame at `depth`.
    fn layout_analyze(&self, laparams: &LAParams, depth: usize) -> PyResult<()> {
        enter_frame(depth)?;
        // textobjs is a list of LTChar objects, i.e.
        // it has all the individual characters in the page.
        let (textobjs, otherobjs) = fsplit(|obj: &LTItem| obj.is_ltchar(), self.objs());
        for obj in &otherobjs {
            obj.analyze_at(laparams, depth + 1)?;
        }
        if textobjs.is_empty() {
            return Ok(());
        }
        let textlines = self.group_objects(laparams, &textobjs)?;
        let (empties, textlines) = fsplit(|obj: &LTItem| obj.is_empty(), textlines);
        for obj in &empties {
            obj.analyze_at(laparams, depth + 1)?;
        }
        let mut textboxes = self.group_textlines(laparams, &textlines)?;
        if laparams.boxes_flow.is_none() {
            for textbox in &textboxes {
                textbox.analyze_at(laparams, depth + 1)?;
            }

            fn getkey(b: &LTItem) -> (i64, f64, f64) {
                if b.is_lttextboxvertical() { (0, -b.x1(), -b.y0()) } else { (1, -b.y0(), b.x0()) }
            }

            sort_by_key_lt(&mut textboxes, getkey, key3_lt);
        } else {
            let groups = self.group_textboxes(laparams, &textboxes)?;
            self.set_groups(Some(groups.clone()));
            let mut assigner = IndexAssigner::new(0);
            for group in &groups {
                group.analyze_at(laparams, depth + 1)?;
                assigner.run_at(group, depth + 1)?;
            }
            textboxes.sort_by_key(|b| b.index());
        }
        let mut objs = textboxes;
        objs.extend(otherobjs);
        objs.extend(empties);
        *self.container()._objs.borrow_mut() = objs;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdfcolor::predefined;
    use crate::pdfinterp::PDFGraphicState;

    fn char_at(text: &str, vertical: bool, textdisp: Value) -> PyResult<LTItem> {
        LTChar_from_font_values(
            (1.0, 0.0, 0.0, 1.0, 0.0, 0.0),
            Value::str("Foo"),
            vertical,
            -0.2,
            10.0,
            1.0,
            0.0,
            PyStr::from(text),
            0.5,
            &textdisp,
            predefined("DeviceGray").unwrap(),
            PDFGraphicState::new().copy(),
        )
    }

    // Expected values from pdfminer.six (a font with fontname "Foo", descent -0.2).
    #[test]
    fn ltchar_vertical_textdisp() {
        let t = Value::tuple;
        let err = |td: Value| {
            let e = char_at("x", true, td).unwrap_err();
            format!("{} {}", e.kind.name(), e.msg)
        };
        assert_eq!(
            err(t(vec![Value::Int(1), Value::Int(2), Value::Int(3)])),
            "ValueError too many values to unpack (expected 2, got 3)"
        );
        assert_eq!(err(t(vec![Value::Int(1)])), "ValueError not enough values to unpack (expected 2, got 1)");
        assert_eq!(err(Value::Int(5)), "AssertionError ");
        assert_eq!(
            err(t(vec![Value::str("a"), Value::Int(3)])),
            "TypeError can't multiply sequence by non-int of type 'float'"
        );
        assert_eq!(
            err(t(vec![Value::bytes(b"x"), Value::Int(3)])),
            "TypeError can't multiply sequence by non-int of type 'float'"
        );
        assert_eq!(
            err(t(vec![Value::Int(3), Value::bytes(b"x")])),
            "TypeError unsupported operand type(s) for -: 'int' and 'bytes'"
        );
        assert_eq!(
            err(t(vec![Value::list(vec![Value::Int(1)]), Value::Int(3)])),
            "TypeError can't multiply sequence by non-int of type 'float'"
        );
        assert_eq!(
            err(t(vec![Value::None, Value::None])),
            "TypeError unsupported operand type(s) for -: 'int' and 'NoneType'"
        );
        let c = char_at("x", true, t(vec![Value::None, Value::Int(880)])).unwrap();
        assert_eq!(
            c.repr(),
            "<LTChar -5.000,1.200,5.000,6.200 matrix=[1.00,0.00,0.00,1.00, (0.00,0.00)] font='Foo' adv=5.0 text='x'>"
        );
        let c = char_at("x", true, t(vec![Value::Bool(true), Value::Int(1)])).unwrap();
        assert_eq!(
            c.repr(),
            "<LTChar -0.010,9.990,9.990,14.990 matrix=[1.00,0.00,0.00,1.00, (0.00,0.00)] font='Foo' adv=5.0 text='x'>"
        );
    }

    #[test]
    fn reprs() {
        let c = char_at("x'\"", false, Value::Int(0)).unwrap();
        assert_eq!(
            c.repr(),
            "<LTChar 0.000,-2.000,5.000,8.000 matrix=[1.00,0.00,0.00,1.00, (0.00,0.00)] font='Foo' adv=5.0 text='x\\'\"'>"
        );
        assert_eq!(c.as_char().unwrap().size, 10.0);
        assert!(c.as_char().unwrap().upright);
        let l = LTTextLineHorizontal(0.1);
        l.add(c);
        assert_eq!(l.repr(), "<LTTextLineHorizontal 0.000,-2.000,5.000,8.000 'x\\'\"'>");
        assert_eq!(LTAnno(PyStr::from("\n")).repr(), "<LTAnno '\\n'>");
        let b = LTTextBoxHorizontal();
        b.add(l);
        assert_eq!(b.repr(), "<LTTextBoxHorizontal(-1) 0.000,-2.000,5.000,8.000 'x\\'\"'>");
        assert_eq!(
            LTPage(1, (0.0, 0.0, 10.0, 10.0), Value::Int(0)).repr(),
            "<LTPage(1) 0.000,0.000,10.000,10.000 rotate=0>"
        );
        assert_eq!(
            LTFigure("F1", (0.0, 0.0, 10.0, 10.0), (1.0, 0.0, 0.0, 1.0, 0.0, 0.0)).repr(),
            "<LTFigure(F1) 0.000,0.000,10.000,10.000 matrix=[1.00,0.00,0.00,1.00, (0.00,0.00)]>"
        );
        let curve = LTCurve(Value::Int(1), vec![(0.0, 0.0), (1.0, 1.0)], false, false, false, None, None, None, None);
        assert_eq!(curve.repr(), "<LTCurve 0.000,0.000,1.000,1.000>");
        assert_eq!(curve.as_curve().unwrap().get_pts(), "0.000,0.000,1.000,1.000");
        assert_eq!(
            LTTextGroupLRTB([]).repr(),
            "<LTTextGroupLRTB 2147483647.000,2147483647.000,-2147483647.000,-2147483647.000>"
        );
        let e = LTExpandableContainer();
        assert_eq!(e.width(), -4294967294.0);
        assert!(e.is_empty());
        assert_eq!(
            LAParams::default().repr(),
            "<LAParams: char_margin=2.0, line_margin=0.5, word_margin=0.1 all_texts=False>"
        );
    }

    #[test]
    fn laparams_validate() {
        let msg = "LAParam boxes_flow should be None, or a number between -1 and +1";
        let mk = |bf: f64| LAParams::new(0.5, 2.0, 0.5, 0.1, Some(bf), false, false);
        for bf in [2.0, f64::NAN, -1.5] {
            let e = mk(bf).unwrap_err();
            assert!(e.is(ExcKind::PDFValueError));
            assert_eq!(e.msg, msg);
        }
        assert!(mk(1.0).is_ok() && mk(-1.0).is_ok());
        let e = LAParams::boxes_flow_from_value(&Value::str("x")).unwrap_err();
        assert!(e.is(ExcKind::PDFTypeError));
        assert_eq!(LAParams::boxes_flow_from_value(&Value::Bool(true)).unwrap(), Some(1.0));
    }

    #[test]
    fn isinstance_follows_mro() {
        let l = LTTextLineHorizontal(0.1);
        assert!(l.is_lttextline() && l.is_lttextcontainer() && l.is_ltexpandablecontainer());
        assert!(l.is_ltcontainer() && l.is_ltcomponent() && l.is_lttext());
        assert!(!l.is_lttextbox() && !l.is_ltlayoutcontainer());
        let p = LTPage(1, (0.0, 0.0, 1.0, 1.0), Value::Int(0));
        assert!(p.is_ltlayoutcontainer() && p.is_ltcontainer() && !p.is_lttext());
        let a = LTAnno(PyStr::from(" "));
        assert!(a.is_lttext() && !a.is_ltcomponent());
        let r = LTRect(Value::Int(0), (0.0, 0.0, 1.0, 1.0), false, false, false, None, None, None, None);
        assert!(r.is_ltcurve() && !r.is_ltline());
    }

    #[test]
    fn deep_tree_drops_without_recursion() {
        let mut g = LTTextBoxHorizontal();
        for _ in 0..200_000 {
            g = LTTextGroupLRTB([g]);
        }
        drop(g);
    }
}
