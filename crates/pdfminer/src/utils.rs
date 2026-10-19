//! Port of `pdfminer/utils.py` (miscellaneous routines).
//!
//! `Plane` and `vecBetweenBoxes` depend on the layout objects and live at the end of this file.

use std::collections::HashSet;
use std::hash::Hash;

use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::fixed;
use crate::pyrt::pystr::PyStr;
use crate::pyrt::value::{float_to_int, int_floordiv, pymax, pymax_iter, pymin, pymin_iter};

/// `INF = (1 << 31) - 1`: PDF still uses 32-bit ints.
pub const INF: i64 = (1 << 31) - 1;

pub type Point = (f64, f64);
pub type Rect = (f64, f64, f64, f64);
pub type Matrix = (f64, f64, f64, f64, f64, f64);

/// `make_compat_bytes(in_str)`: encodes to UTF-8.
pub fn make_compat_bytes(in_str: &str) -> Vec<u8> {
    in_str.as_bytes().to_vec()
}

/// `make_compat_str(o)` for `bytes`: Python guesses the encoding with charset_normalizer. The
/// port decodes UTF-8 and otherwise returns `str(o)` (recorded divergence: see
/// docs/DIVERGENCES.md; pdfminer only reaches this for font names that are not `str`).
pub fn make_compat_str_bytes(o: &[u8]) -> String {
    match std::str::from_utf8(o) {
        Ok(s) => s.to_string(),
        Err(_) => crate::pyrt::fmt::bytes_repr(o),
    }
}

/// `shorten_str(s, size)`.
pub fn shorten_str(s: &str, size: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if size < 7 {
        return chars[..size.min(chars.len())].iter().collect();
    }
    if chars.len() > size {
        let length = (size - 5) / 2;
        let head: String = chars[..length].iter().collect();
        let tail: String = chars[chars.len() - length..].iter().collect();
        format!("{head} ... {tail}")
    } else {
        s.to_string()
    }
}

/// `compatible_encode_method(b, "utf-8", "ignore")` for `bytes`.
pub fn compatible_encode_method(b: &[u8]) -> String {
    crate::pyrt::codecs::utf8_decode_ignore(b)
}

/// `paeth_predictor(left, above, upper_left)`.
pub fn paeth_predictor(left: i64, above: i64, upper_left: i64) -> i64 {
    // From http://www.libpng.org/pub/png/spec/1.2/PNG-Filters.html
    // Initial estimate
    let p = left + above - upper_left;
    // Distances to a,b,c
    let pa = (p - left).abs();
    let pb = (p - above).abs();
    let pc = (p - upper_left).abs();
    // Return nearest of a,b,c breaking ties in order a,b,c
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        above
    } else {
        upper_left
    }
}

fn index_error() -> PyErr {
    PyErr::new(ExcKind::IndexError, "index out of range")
}

/// `seq[i]` with Python's bounds (negative indices count from the end).
fn at<T: Copy>(seq: &[T], i: i64) -> PyResult<T> {
    let n = seq.len() as i64;
    let j = if i < 0 { i + n } else { i };
    if j < 0 || j >= n {
        return Err(index_error());
    }
    Ok(seq[j as usize])
}

/// `range(start, stop, step)` as a Vec of indices (Python raises `ValueError` for step 0).
fn py_range(start: i64, stop: i64, step: i64) -> PyResult<Vec<i64>> {
    if step == 0 {
        return Err(PyErr::new(ExcKind::ValueError, "range() arg 3 must not be zero"));
    }
    let mut out = Vec::new();
    let mut i = start;
    if step > 0 {
        while i < stop {
            out.push(i);
            i += step;
        }
    } else {
        while i > stop {
            out.push(i);
            i += step;
        }
    }
    Ok(out)
}

/// `apply_tiff_predictor(colors, columns, bitspercomponent, data)`: reverses TIFF predictor 2.
pub fn apply_tiff_predictor(colors: i64, columns: i64, bitspercomponent: i64, data: &[u8]) -> PyResult<Vec<u8>> {
    if bitspercomponent != 8 {
        let error_msg = format!("Unsupported `bitspercomponent': {bitspercomponent}");
        return Err(PyErr::new(ExcKind::PDFValueError, error_msg));
    }
    let bpp = colors * int_floordiv(bitspercomponent, 8);
    let nbytes = columns * bpp;
    let mut buf: Vec<u8> = Vec::new();
    for scanline_i in py_range(0, data.len() as i64, nbytes)? {
        let mut raw: Vec<i64> = Vec::new();
        for i in 0..nbytes.max(0) {
            let mut new_value = at(data, scanline_i + i)? as i64;
            if i >= bpp {
                new_value += at(&raw, i - bpp)?;
                new_value = new_value.rem_euclid(256);
            }
            raw.push(new_value);
        }
        // bytes(buf): values are already in 0..256
        buf.extend(raw.iter().map(|&v| v as u8));
    }
    Ok(buf)
}

/// `apply_png_predictor(pred, colors, columns, bitspercomponent, data)`: reverses the PNG
/// predictor.
pub fn apply_png_predictor(
    _pred: i64,
    colors: i64,
    columns: i64,
    bitspercomponent: i64,
    data: &[u8],
) -> PyResult<Vec<u8>> {
    if bitspercomponent != 8 && bitspercomponent != 1 {
        let msg = format!("Unsupported `bitspercomponent': {bitspercomponent}");
        return Err(PyErr::new(ExcKind::PDFValueError, msg));
    }
    let nbytes = int_floordiv(colors * columns * bitspercomponent, 8);
    let bpp = int_floordiv(colors * bitspercomponent, 8); // number of bytes per complete pixel
    let mut buf: Vec<u8> = Vec::new();
    let mut line_above: Vec<i64> = vec![0; columns.max(0) as usize];
    for scanline_i in py_range(0, data.len() as i64, nbytes + 1)? {
        let filter_type = at(data, scanline_i)?;
        let start = (scanline_i + 1) as usize;
        let end = (scanline_i + 1 + nbytes).max(0) as usize;
        let line_encoded: &[u8] = &data[start.min(data.len())..end.min(data.len()).max(start.min(data.len()))];
        let mut raw: Vec<i64> = Vec::new();

        match filter_type {
            // Filter type 0: None
            0 => raw = line_encoded.iter().map(|&b| b as i64).collect(),
            // Filter type 1: Sub
            1 => {
                for (j, &sub_x) in line_encoded.iter().enumerate() {
                    let j = j as i64;
                    let raw_x_bpp = if j - bpp < 0 { 0 } else { at(&raw, j - bpp)? };
                    let raw_x = (sub_x as i64 + raw_x_bpp) & 255;
                    raw.push(raw_x);
                }
            }
            // Filter type 2: Up
            2 => {
                for (&up_x, &prior_x) in line_encoded.iter().zip(line_above.iter()) {
                    let raw_x = (up_x as i64 + prior_x) & 255;
                    raw.push(raw_x);
                }
            }
            // Filter type 3: Average
            3 => {
                for (j, &average_x) in line_encoded.iter().enumerate() {
                    let j = j as i64;
                    let raw_x_bpp = if j - bpp < 0 { 0 } else { at(&raw, j - bpp)? };
                    let prior_x = at(&line_above, j)?;
                    let raw_x = (average_x as i64 + int_floordiv(raw_x_bpp + prior_x, 2)) & 255;
                    raw.push(raw_x);
                }
            }
            // Filter type 4: Paeth
            4 => {
                for (j, &paeth_x) in line_encoded.iter().enumerate() {
                    let j = j as i64;
                    let (raw_x_bpp, prior_x_bpp) = if j - bpp < 0 {
                        (0, 0)
                    } else {
                        (at(&raw, j - bpp)?, at(&line_above, j - bpp)?)
                    };
                    let prior_x = at(&line_above, j)?;
                    let paeth = paeth_predictor(raw_x_bpp, prior_x, prior_x_bpp);
                    let raw_x = (paeth_x as i64 + paeth) & 255;
                    raw.push(raw_x);
                }
            }
            _ => {
                return Err(PyErr::new(
                    ExcKind::PDFValueError,
                    format!("Unsupported predictor value: {filter_type}"),
                ));
            }
        }
        buf.extend(raw.iter().map(|&v| v as u8));
        line_above = raw;
    }
    Ok(buf)
}

//  Matrix operations
pub const MATRIX_IDENTITY: Matrix = (1.0, 0.0, 0.0, 1.0, 0.0, 0.0);

/// `parse_rect(o)`: a `ValueError` (wrong length, or `float()` of a bad string) becomes
/// `PDFValueError`; a `TypeError` (`float(None)`) propagates as is.
pub fn parse_rect(o: &[crate::pyrt::Value]) -> PyResult<Rect> {
    let conv = || -> PyResult<Rect> {
        if o.len() != 4 {
            return Err(PyErr::new(ExcKind::ValueError, "wrong number of values to unpack"));
        }
        let f = |v: &crate::pyrt::Value| crate::pyrt::value::py_float(v);
        Ok((f(&o[0])?, f(&o[1])?, f(&o[2])?, f(&o[3])?))
    };
    conv().map_err(|e| {
        if e.is(ExcKind::ValueError) { PyErr::new(ExcKind::PDFValueError, "Could not parse rectangle") } else { e }
    })
}

/// `mult_matrix(m1, m0)`: returns the multiplication of two matrices.
pub fn mult_matrix(m1: Matrix, m0: Matrix) -> Matrix {
    let (a1, b1, c1, d1, e1, f1) = m1;
    let (a0, b0, c0, d0, e0, f0) = m0;
    (
        a0 * a1 + c0 * b1,
        b0 * a1 + d0 * b1,
        a0 * c1 + c0 * d1,
        b0 * c1 + d0 * d1,
        a0 * e1 + c0 * f1 + e0,
        b0 * e1 + d0 * f1 + f0,
    )
}

/// `translate_matrix(m, v)`: moves the origin of `m` to `v` in its own coordinate system.
pub fn translate_matrix(m: Matrix, v: Point) -> Matrix {
    let (a, b, c, d, e, f) = m;
    let (x, y) = v;
    (a, b, c, d, x * a + y * c + e, x * b + y * d + f)
}

/// `apply_matrix_pt(m, v)`.
pub fn apply_matrix_pt(m: Matrix, v: Point) -> Point {
    let (a, b, c, d, e, f) = m;
    let (x, y) = v;
    (a * x + c * y + e, b * x + d * y + f)
}

/// `apply_matrix_rect(m, rect)`: the upright rectangle that fits the transformed rectangle.
pub fn apply_matrix_rect(m: Matrix, rect: Rect) -> Rect {
    let (x0, y0, x1, y1) = rect;
    let left_bottom = (x0, y0);
    let right_bottom = (x1, y0);
    let right_top = (x1, y1);
    let left_top = (x0, y1);

    let (left1, bottom1) = apply_matrix_pt(m, left_bottom);
    let (right1, bottom2) = apply_matrix_pt(m, right_bottom);
    let (right2, top1) = apply_matrix_pt(m, right_top);
    let (left2, top2) = apply_matrix_pt(m, left_top);

    (
        pymin_iter([left1, left2, right1, right2]),
        pymin_iter([bottom1, bottom2, top1, top2]),
        pymax_iter([left1, left2, right1, right2]),
        pymax_iter([bottom1, bottom2, top1, top2]),
    )
}

/// `apply_matrix_norm(m, v)`: `apply_matrix_pt(m, v) - apply_matrix_pt(m, (0, 0))`.
pub fn apply_matrix_norm(m: Matrix, v: Point) -> Point {
    let (a, b, c, d, _e, _f) = m;
    let (p, q) = v;
    (a * p + c * q, b * p + d * q)
}

/// `uniq(objs)`: eliminates duplicated elements, keeping the first.
pub fn uniq<T: Hash + Eq + Clone>(objs: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut done = HashSet::new();
    let mut out = Vec::new();
    for obj in objs {
        if done.contains(&obj) {
            continue;
        }
        done.insert(obj.clone());
        out.push(obj);
    }
    out
}

/// `fsplit(pred, objs)`: splits a list into two classes according to the predicate.
pub fn fsplit<T>(pred: impl Fn(&T) -> bool, objs: impl IntoIterator<Item = T>) -> (Vec<T>, Vec<T>) {
    let mut t = Vec::new();
    let mut f = Vec::new();
    for obj in objs {
        if pred(&obj) {
            t.push(obj);
        } else {
            f.push(obj);
        }
    }
    (t, f)
}

/// `drange(v0, v1, d)`: `range(int(v0) // d, int(v1 + d) // d)`.
pub fn drange(v0: f64, v1: f64, d: i64) -> PyResult<std::ops::Range<i64>> {
    let a = int_floordiv(float_to_int(v0)?, d);
    let b = int_floordiv(float_to_int(v1 + d as f64)?, d);
    Ok(a..b)
}

/// `get_bound(pts)`: the smallest rectangle that covers all the points.
pub fn get_bound(pts: impl IntoIterator<Item = Point>) -> Rect {
    let inf = INF as f64;
    let (mut x0, mut y0, mut x1, mut y1) = (inf, inf, -inf, -inf);
    for (x, y) in pts {
        x0 = pymin(x0, x);
        y0 = pymin(y0, y);
        x1 = pymax(x1, x);
        y1 = pymax(y1, y);
    }
    (x0, y0, x1, y1)
}

/// `pick(seq, func)`: the object with the highest `func(obj)` (the first one on ties).
pub fn pick<T: Clone>(seq: impl IntoIterator<Item = T>, func: impl Fn(&T) -> f64, maxobj: Option<T>) -> Option<T> {
    let mut maxscore: Option<f64> = None;
    let mut maxobj = maxobj;
    for obj in seq {
        let score = func(&obj);
        if maxscore.is_none_or(|m| m < score) {
            maxscore = Some(score);
            maxobj = Some(obj);
        }
    }
    maxobj
}

/// `choplist(n, seq)`: groups every n elements (a trailing partial group is dropped).
pub fn choplist<T>(n: usize, seq: impl IntoIterator<Item = T>) -> Vec<Vec<T>> {
    let mut out = Vec::new();
    let mut r = Vec::with_capacity(n);
    for x in seq {
        r.push(x);
        if r.len() == n {
            out.push(std::mem::replace(&mut r, Vec::with_capacity(n)));
        }
    }
    out
}

/// `nunpack(s, default)`: unpacks a variable-length unsigned big-endian integer. Python returns
/// a big int beyond 8 bytes; the port keeps the low 63 bits there (recorded divergence).
pub fn nunpack(s: &[u8], default: i64) -> i64 {
    if s.is_empty() {
        return default;
    }
    let mut v: u64 = 0;
    for &b in s {
        v = (v << 8) | b as u64;
    }
    (v & (i64::MAX as u64)) as i64
}

#[rustfmt::skip]
pub const PDF_DOC_ENCODING: [u16; 256] = [
    0x0000, 0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007,
    0x0008, 0x0009, 0x000A, 0x000B, 0x000C, 0x000D, 0x000E, 0x000F,
    0x0010, 0x0011, 0x0012, 0x0013, 0x0014, 0x0015, 0x0017, 0x0017,
    0x02D8, 0x02C7, 0x02C6, 0x02D9, 0x02DD, 0x02DB, 0x02DA, 0x02DC,
    0x0020, 0x0021, 0x0022, 0x0023, 0x0024, 0x0025, 0x0026, 0x0027,
    0x0028, 0x0029, 0x002A, 0x002B, 0x002C, 0x002D, 0x002E, 0x002F,
    0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037,
    0x0038, 0x0039, 0x003A, 0x003B, 0x003C, 0x003D, 0x003E, 0x003F,
    0x0040, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047,
    0x0048, 0x0049, 0x004A, 0x004B, 0x004C, 0x004D, 0x004E, 0x004F,
    0x0050, 0x0051, 0x0052, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057,
    0x0058, 0x0059, 0x005A, 0x005B, 0x005C, 0x005D, 0x005E, 0x005F,
    0x0060, 0x0061, 0x0062, 0x0063, 0x0064, 0x0065, 0x0066, 0x0067,
    0x0068, 0x0069, 0x006A, 0x006B, 0x006C, 0x006D, 0x006E, 0x006F,
    0x0070, 0x0071, 0x0072, 0x0073, 0x0074, 0x0075, 0x0076, 0x0077,
    0x0078, 0x0079, 0x007A, 0x007B, 0x007C, 0x007D, 0x007E, 0x0000,
    0x2022, 0x2020, 0x2021, 0x2026, 0x2014, 0x2013, 0x0192, 0x2044,
    0x2039, 0x203A, 0x2212, 0x2030, 0x201E, 0x201C, 0x201D, 0x2018,
    0x2019, 0x201A, 0x2122, 0xFB01, 0xFB02, 0x0141, 0x0152, 0x0160,
    0x0178, 0x017D, 0x0131, 0x0142, 0x0153, 0x0161, 0x017E, 0x0000,
    0x20AC, 0x00A1, 0x00A2, 0x00A3, 0x00A4, 0x00A5, 0x00A6, 0x00A7,
    0x00A8, 0x00A9, 0x00AA, 0x00AB, 0x00AC, 0x0000, 0x00AE, 0x00AF,
    0x00B0, 0x00B1, 0x00B2, 0x00B3, 0x00B4, 0x00B5, 0x00B6, 0x00B7,
    0x00B8, 0x00B9, 0x00BA, 0x00BB, 0x00BC, 0x00BD, 0x00BE, 0x00BF,
    0x00C0, 0x00C1, 0x00C2, 0x00C3, 0x00C4, 0x00C5, 0x00C6, 0x00C7,
    0x00C8, 0x00C9, 0x00CA, 0x00CB, 0x00CC, 0x00CD, 0x00CE, 0x00CF,
    0x00D0, 0x00D1, 0x00D2, 0x00D3, 0x00D4, 0x00D5, 0x00D6, 0x00D7,
    0x00D8, 0x00D9, 0x00DA, 0x00DB, 0x00DC, 0x00DD, 0x00DE, 0x00DF,
    0x00E0, 0x00E1, 0x00E2, 0x00E3, 0x00E4, 0x00E5, 0x00E6, 0x00E7,
    0x00E8, 0x00E9, 0x00EA, 0x00EB, 0x00EC, 0x00ED, 0x00EE, 0x00EF,
    0x00F0, 0x00F1, 0x00F2, 0x00F3, 0x00F4, 0x00F5, 0x00F6, 0x00F7,
    0x00F8, 0x00F9, 0x00FA, 0x00FB, 0x00FC, 0x00FD, 0x00FE, 0x00FF,
];

/// `decode_text(s)`: decodes a PDFDocEncoding (or UTF-16BE with BOM) string.
pub fn decode_text(s: &[u8]) -> String {
    if s.starts_with(b"\xfe\xff") {
        PyStr::decode_utf16be_ignore(&s[2..]).as_str().expect("utf-16 'ignore' yields no surrogates").to_string()
    } else {
        s.iter().map(|&c| char::from_u32(PDF_DOC_ENCODING[c as usize] as u32).unwrap()).collect()
    }
}

/// `enc(x)`: `html.escape(x)` (quotes included) for SGML/XML/HTML.
pub fn enc(x: &str) -> String {
    let mut out = String::with_capacity(x.len());
    for c in x.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// `enc(x)` for text that may hold surrogates.
pub fn enc_pystr(x: &PyStr) -> PyStr {
    let mut out = PyStr::new();
    for cp in x.code_points() {
        match cp {
            0x26 => out.push_str("&amp;"),
            0x3C => out.push_str("&lt;"),
            0x3E => out.push_str("&gt;"),
            0x22 => out.push_str("&quot;"),
            0x27 => out.push_str("&#x27;"),
            _ => out.push_cp(cp),
        }
    }
    out
}

/// `bbox2str(bbox)`.
pub fn bbox2str(bbox: Rect) -> String {
    let (x0, y0, x1, y1) = bbox;
    format!("{},{},{},{}", fixed(x0, 3), fixed(y0, 3), fixed(x1, 3), fixed(y1, 3))
}

/// `matrix2str(m)`.
pub fn matrix2str(m: Matrix) -> String {
    let (a, b, c, d, e, f) = m;
    format!(
        "[{},{},{},{}, ({},{})]",
        fixed(a, 2),
        fixed(b, 2),
        fixed(c, 2),
        fixed(d, 2),
        fixed(e, 2),
        fixed(f, 2)
    )
}

const ROMAN_ONES: [&str; 4] = ["i", "x", "c", "m"];
const ROMAN_FIVES: [&str; 3] = ["v", "l", "d"];

/// `format_int_roman(value)`: lowercase Roman numerals (`0 < value < 4000`).
pub fn format_int_roman(mut value: i64) -> PyResult<String> {
    if !(0 < value && value < 4000) {
        return Err(PyErr::new(ExcKind::AssertionError, ""));
    }
    let mut result: Vec<String> = Vec::new();
    let mut index = 0;
    while value != 0 {
        let mut remainder = value % 10;
        value /= 10;
        if remainder == 9 {
            result.insert(0, ROMAN_ONES[index].into());
            result.insert(1, ROMAN_ONES[index + 1].into());
        } else if remainder == 4 {
            result.insert(0, ROMAN_ONES[index].into());
            result.insert(1, ROMAN_FIVES[index].into());
        } else {
            let over_five = remainder >= 5;
            if over_five {
                result.insert(0, ROMAN_FIVES[index].into());
                remainder -= 5;
            }
            result.insert(if over_five { 1 } else { 0 }, ROMAN_ONES[index].repeat(remainder as usize));
        }
        index += 1;
    }
    Ok(result.join(""))
}

/// `format_int_alpha(value)`: lowercase letters a-z, aa-zz, ... (`value > 0`).
pub fn format_int_alpha(mut value: i64) -> PyResult<String> {
    if value <= 0 {
        return Err(PyErr::new(ExcKind::AssertionError, ""));
    }
    let mut result: Vec<char> = Vec::new();
    while value != 0 {
        let remainder = (value - 1) % 26;
        value = (value - 1) / 26;
        result.push((b'a' + remainder as u8) as char);
    }
    result.reverse();
    Ok(result.into_iter().collect())
}

/// `unpad_aes(padded)`: removes the block padding of PDF 1.7 section 7.6.2 when it is valid.
pub fn unpad_aes(padded: &[u8]) -> &[u8] {
    if padded.is_empty() {
        return padded;
    }
    // Check for a potential padding byte (bytes are unsigned)
    let padding = padded[padded.len() - 1] as usize;
    if padding > 16 {
        return padded;
    }
    // A valid padding byte is the length of the padding
    if padding > padded.len() {
        // Obviously invalid
        return padded;
    }
    // Every byte of padding is equal to the length of padding
    // (`padded[-0:]` is the whole string when padding == 0.)
    let tail = if padding == 0 { padded } else { &padded[padded.len() - padding..] };
    if tail.iter().all(|&x| x as usize == padding) {
        // padded[:-padding]; with padding == 0 this is padded[:0] == b"".
        if padding == 0 {
            return &padded[..0];
        }
        return &padded[..padded.len() - padding];
    }
    padded
}

// --- Plane and vecBetweenBoxes (they need the layout objects) -------------------------------
