//! Port of `pdfminer/pdfcolor.py`.

use std::rc::Rc;

use crate::psparser::{LIT, PSLiteral};

pub fn literal_device_gray() -> PSLiteral {
    LIT("DeviceGray")
}
pub fn literal_device_rgb() -> PSLiteral {
    LIT("DeviceRGB")
}
pub fn literal_device_cmyk() -> PSLiteral {
    LIT("DeviceCMYK")
}
// Abbreviations for inline images
pub fn literal_inline_device_gray() -> PSLiteral {
    LIT("G")
}
pub fn literal_inline_device_rgb() -> PSLiteral {
    LIT("RGB")
}
pub fn literal_inline_device_cmyk() -> PSLiteral {
    LIT("CMYK")
}

/// `PDFColorSpace(name, ncomponents)`. Shared (`Rc`) because Python shares the instances of
/// `PREDEFINED_COLORSPACE` and the resource color spaces between states.
#[derive(Clone, Debug, PartialEq)]
pub struct PDFColorSpace(pub Rc<PDFColorSpaceInner>);

#[derive(Debug, PartialEq)]
pub struct PDFColorSpaceInner {
    pub name: String,
    pub ncomponents: i64,
}

impl PDFColorSpace {
    pub fn new(name: &str, ncomponents: i64) -> Self {
        PDFColorSpace(Rc::new(PDFColorSpaceInner { name: name.to_string(), ncomponents }))
    }

    pub fn name(&self) -> &str {
        &self.0.name
    }

    pub fn ncomponents(&self) -> i64 {
        self.0.ncomponents
    }

    pub fn repr(&self) -> String {
        format!("<PDFColorSpace: {}, ncomponents={}>", self.0.name, self.0.ncomponents)
    }
}

/// The entries of `PREDEFINED_COLORSPACE`, in order.
pub const PREDEFINED_COLORSPACE_ENTRIES: &[(&str, i64)] = &[
    ("DeviceGray", 1), // default value first
    ("CalRGB", 3),
    ("CalGray", 1),
    ("Lab", 3),
    ("DeviceRGB", 3),
    ("DeviceCMYK", 4),
    ("Separation", 1),
    ("Indexed", 1),
    ("Pattern", 1),
];

thread_local! {
    static PREDEFINED: Vec<(&'static str, PDFColorSpace)> =
        PREDEFINED_COLORSPACE_ENTRIES.iter().map(|&(n, c)| (n, PDFColorSpace::new(n, c))).collect();
}

/// `PREDEFINED_COLORSPACE` (an ordered dict name -> shared instance).
pub fn predefined_colorspace() -> Vec<(&'static str, PDFColorSpace)> {
    PREDEFINED.with(|p| p.clone())
}

/// `PREDEFINED_COLORSPACE[name]`.
pub fn predefined(name: &str) -> Option<PDFColorSpace> {
    PREDEFINED.with(|p| p.iter().find(|(n, _)| *n == name).map(|(_, cs)| cs.clone()))
}
