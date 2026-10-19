//! Port of `pdfminer/pdfinterp.py`: only the text and graphic states so far.

use std::cell::RefCell;
use std::rc::Rc;

use crate::pdfcolor::{PDFColorSpace, predefined};
use crate::pdffont::PDFFont;
use crate::pyrt::fmt::float_repr;
use crate::pyrt::value::Value;
use crate::utils::{MATRIX_IDENTITY, Matrix, Point};

/// `PDFTextState`.
#[derive(Clone)]
pub struct PDFTextState {
    pub font: Option<Rc<PDFFont>>,
    pub fontsize: f64,
    pub charspace: f64,
    pub wordspace: f64,
    pub scaling: f64,
    pub leading: f64,
    pub render: i64,
    pub rise: f64,
    pub matrix: Matrix,
    pub linematrix: Point,
}

impl Default for PDFTextState {
    fn default() -> Self {
        Self::new()
    }
}

impl PDFTextState {
    pub fn new() -> Self {
        let mut s = PDFTextState {
            font: None,
            fontsize: 0.0,
            charspace: 0.0,
            wordspace: 0.0,
            scaling: 100.0,
            leading: 0.0,
            render: 0,
            rise: 0.0,
            matrix: MATRIX_IDENTITY,
            linematrix: (0.0, 0.0),
        };
        s.reset();
        s
    }

    /// `textstate.copy()`.
    pub fn copy(&self) -> Self {
        self.clone()
    }

    pub fn reset(&mut self) {
        self.matrix = MATRIX_IDENTITY;
        self.linematrix = (0.0, 0.0);
    }
}

/// A standard color: gray, RGB or CMYK (`StandardColor`).
#[derive(Clone, Debug, PartialEq)]
pub enum StandardColor {
    /// The initial color: Python's int `0` (printed `0`, not `0.0`, by the XML converter).
    /// Every operator that sets a color produces floats.
    IntGray(i64),
    Gray(f64),
    Rgb(f64, f64, f64),
    Cmyk(f64, f64, f64, f64),
}

/// `Color`: a standard color, a pattern name (colored pattern), or `(base_color, pattern_name)`
/// (uncolored pattern).
#[derive(Clone, Debug, PartialEq)]
pub enum Color {
    Standard(StandardColor),
    Pattern(String),
    UncoloredPattern(StandardColor, String),
}

impl StandardColor {
    pub fn repr(&self) -> String {
        match self {
            StandardColor::IntGray(g) => g.to_string(),
            StandardColor::Gray(g) => float_repr(*g),
            StandardColor::Rgb(r, g, b) => format!("({}, {}, {})", float_repr(*r), float_repr(*g), float_repr(*b)),
            StandardColor::Cmyk(c, m, y, k) => {
                format!("({}, {}, {}, {})", float_repr(*c), float_repr(*m), float_repr(*y), float_repr(*k))
            }
        }
    }
}

/// A `PDFGraphicState` object, shared like Python shares it.
///
/// `LTChar.graphicstate` keeps a reference to the interpreter's current graphic state, and the
/// interpreter keeps mutating that same object (`do_rg` sets `ncolor` in place) until `Q`
/// replaces it with the copy saved by `q`. So a character's `graphicstate.ncolor` is the color at
/// the end of its graphics-state block, not at the time it was drawn. Callers (FuelFwd's
/// invisible-text check) read it, so the port shares the state the same way: mutate through the
/// `RefCell`, and `copy()` makes a new `Rc`.
pub type PDFGraphicStateRef = Rc<RefCell<PDFGraphicState>>;

/// `PDFGraphicState`.
#[derive(Clone)]
pub struct PDFGraphicState {
    /// The int `0` until `w` sets a float: the XML converter prints it with `str()`.
    pub linewidth: Value,
    pub linecap: Value,
    pub linejoin: Value,
    pub miterlimit: Value,
    /// `(dash, phase)` or `None`.
    pub dash: Option<(Value, Value)>,
    pub intent: Value,
    pub flatness: Value,
    /// stroking color
    pub scolor: Color,
    pub scs: PDFColorSpace,
    /// non stroking color
    pub ncolor: Color,
    pub ncs: PDFColorSpace,
}

impl Default for PDFGraphicState {
    fn default() -> Self {
        Self::new()
    }
}

impl PDFGraphicState {
    pub fn new() -> Self {
        PDFGraphicState {
            linewidth: Value::Int(0),
            linecap: Value::None,
            linejoin: Value::None,
            miterlimit: Value::None,
            dash: None,
            intent: Value::None,
            flatness: Value::None,
            scolor: Color::Standard(StandardColor::IntGray(0)),
            scs: predefined("DeviceGray").unwrap(),
            ncolor: Color::Standard(StandardColor::IntGray(0)),
            ncs: predefined("DeviceGray").unwrap(),
        }
    }

    /// `graphicstate.copy()`: a new, separate object.
    pub fn copy(&self) -> PDFGraphicStateRef {
        Rc::new(RefCell::new(self.clone()))
    }
}
