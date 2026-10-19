//! Port of `pdfminer/image.py`.
//!
//! Pillow is an optional dependency of pdfminer.six that the reference environment does not
//! install, so the port reproduces pdfminer.six without Pillow: the branches that need it
//! (`_save_bytes`, CMYK JPEGs, JPEG 2000) create the (empty) output file and raise
//! `ImportError(PIL_ERROR_MESSAGE)`. DIVERGE(pillow): with Pillow installed, Python converts
//! those images instead.
//!
//! Files are written the way Python's buffered file writes them: everything written before an
//! exception is in the file.

use std::rc::Rc;

use crate::jbig2::jbig2_convert;
use crate::layout::LTItem;
use crate::pdfcolor::{
    literal_device_cmyk, literal_device_gray, literal_device_rgb, literal_inline_device_gray, literal_inline_device_rgb,
};
use crate::pdftypes::{
    LITERALS_DCT_DECODE, LITERALS_FLATE_DECODE, LITERALS_JBIG2_DECODE, LITERALS_JPX_DECODE, PDFStream, in_literals,
};
use crate::psparser::PSLiteral;
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::textio::{os_error, os_error_path};
use crate::pyrt::value::{Value, py_add, py_eq, py_floordiv, py_mul, py_truediv};

pub const PIL_ERROR_MESSAGE: &str = "Could not import Pillow. This dependency of pdfminer.six is not installed by default. You need it to to save jpg images to a file. Install it with `pip install 'pdfminer.six[image]'`";

fn import_error() -> PyErr {
    PyErr::new(ExcKind::ImportError, PIL_ERROR_MESSAGE)
}

/// `align32(x)`.
pub fn align32(x: &Value) -> PyResult<Value> {
    py_mul(&py_floordiv(&py_add(x, &Value::Int(3))?, &Value::Int(4))?, &Value::Int(4))
}

/// A file opened with `open(path, "wb")`: kept in memory and written out when closed (the
/// `with` block ends, normally or by an exception), like Python's buffered file.
struct OutFile {
    path: String,
    data: Vec<u8>,
    pos: usize,
}

impl OutFile {
    /// `open(path, "wb")`: creates (or truncates) the file.
    fn open(path: &str) -> PyResult<OutFile> {
        std::fs::File::create(path).map_err(|e| os_error_path(&e, path))?;
        Ok(OutFile { path: path.to_string(), data: Vec::new(), pos: 0 })
    }

    fn write(&mut self, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        let end = self.pos + b.len();
        if self.data.len() < end {
            self.data.resize(end, 0);
        }
        self.data[self.pos..end].copy_from_slice(b);
        self.pos = end;
    }

    fn tell(&self) -> usize {
        self.pos
    }

    fn seek(&mut self, pos: i64) -> PyResult<()> {
        if pos < 0 {
            return Err(PyErr::new(ExcKind::OSError, "[Errno 22] Invalid argument"));
        }
        self.pos = pos as usize;
        Ok(())
    }

    /// Closes the file: writes what was written, then passes `r` on (the first error wins).
    fn close<T>(self, r: PyResult<T>) -> PyResult<T> {
        let w = std::fs::write(&self.path, &self.data).map_err(|e| os_error(&e));
        let v = r?;
        w?;
        Ok(v)
    }
}

/// `struct.pack` of one integer field (`B`, `H`, `I`, `i`), with Python's errors.
fn pack_int(fmt: char, v: &Value) -> PyResult<Vec<u8>> {
    let Some(x) = (if v.is_int() { v.as_int() } else { None }) else {
        return Err(PyErr::new(ExcKind::StructError, "required argument is not an integer"));
    };
    let (lo, hi, n): (i64, i64, usize) = match fmt {
        'B' => (0, 255, 1),
        'H' => (0, 65535, 2),
        'I' => (0, 4294967295, 4),
        'i' => (-2147483648, 2147483647, 4),
        _ => unreachable!("format {fmt}"),
    };
    if x < lo || x > hi {
        return Err(PyErr::new(ExcKind::StructError, format!("'{fmt}' format requires {lo} <= number <= {hi}")));
    }
    Ok(x.to_le_bytes()[..n].to_vec())
}

/// `struct.pack("<" + fmt, *args)` for integer formats.
fn pack(fmt: &str, args: &[Value]) -> PyResult<Vec<u8>> {
    let mut out = Vec::new();
    for (c, v) in fmt.chars().zip(args) {
        out.extend(pack_int(c, v)?);
    }
    Ok(out)
}

/// A Python int used as an index (`range(n)`, a slice bound): `TypeError` otherwise.
fn index_int(v: &Value, slice: bool) -> PyResult<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        Value::Bool(b) => Ok(*b as i64),
        // Slice bounds clamp; `range(n)` of a big n runs out of memory or time either way.
        Value::BigInt(b) => Ok(if num_traits::Signed::is_negative(&**b) { i64::MIN } else { i64::MAX }),
        _ if slice => {
            Err(PyErr::new(ExcKind::TypeError, "slice indices must be integers or None or have an __index__ method"))
        }
        _ => Err(PyErr::new(
            ExcKind::TypeError,
            format!("'{}' object cannot be interpreted as an integer", v.type_name()),
        )),
    }
}

/// `data[i:j]` with Python's clamping.
fn slice(data: &[u8], i: i64, j: i64) -> &[u8] {
    let n = data.len() as i64;
    let norm = |k: i64| -> usize { (if k < 0 { (k + n).max(0) } else { k.min(n) }) as usize };
    let (a, b) = (norm(i), norm(j));
    if a >= b { &[] } else { &data[a..b] }
}

pub struct BMPWriter {
    pub bits: Value,
    pub width: Value,
    pub height: Value,
    pub linesize: Value,
    pub datasize: Value,
    pub pos0: usize,
    pub pos1: i64,
}

impl BMPWriter {
    fn new(fp: &mut OutFile, bits: &Value, width: &Value, height: &Value) -> PyResult<BMPWriter> {
        let ncols: i64 = if py_eq(bits, &Value::Int(1)) {
            2
        } else if py_eq(bits, &Value::Int(8)) {
            256
        } else if py_eq(bits, &Value::Int(24)) {
            0
        } else {
            return Err(PyErr::new(ExcKind::PDFValueError, bits.py_str()));
        };
        let linesize = align32(&py_floordiv(&py_add(&py_mul(width, bits)?, &Value::Int(7))?, &Value::Int(8))?)?;
        let datasize = py_mul(&linesize, height)?;
        let headersize = 14 + 40 + ncols * 4;
        let info = pack(
            "IiiHHIIIIII",
            &[
                Value::Int(40),
                width.clone(),
                height.clone(),
                Value::Int(1),
                bits.clone(),
                Value::Int(0),
                datasize.clone(),
                Value::Int(0),
                Value::Int(0),
                Value::Int(ncols),
                Value::Int(0),
            ],
        )?;
        assert_eq!(info.len(), 40);
        let mut header = b"BM".to_vec();
        header.extend(pack(
            "IHHI",
            &[py_add(&Value::Int(headersize), &datasize)?, Value::Int(0), Value::Int(0), Value::Int(headersize)],
        )?);
        assert_eq!(header.len(), 14);
        fp.write(&header);
        fp.write(&info);
        if ncols == 2 {
            // B&W color table
            for i in [0u8, 255] {
                fp.write(&[i, i, i, 0]);
            }
        } else if ncols == 256 {
            // grayscale color table
            for i in 0..=255u8 {
                fp.write(&[i, i, i, 0]);
            }
        }
        let pos0 = fp.tell();
        // `datasize` passed the 'I' check: an int.
        let pos1 = pos0 as i64 + datasize.as_int().expect("int");
        Ok(BMPWriter {
            bits: bits.clone(),
            width: width.clone(),
            height: height.clone(),
            linesize,
            datasize,
            pos0,
            pos1,
        })
    }

    fn write_line(&self, fp: &mut OutFile, y: i64, data: &[u8]) -> PyResult<()> {
        let linesize = self.linesize.as_int().expect("int");
        fp.seek(self.pos1 - (y + 1) * linesize)?;
        fp.write(data);
        Ok(())
    }
}

/// `x in image.colorspace` for a literal `x`.
fn colorspace_has(colorspace: &Value, lit: &PSLiteral) -> bool {
    match colorspace {
        Value::List(l) => l.borrow().iter().any(|v| matches!(v, Value::Literal(x) if x == lit)),
        Value::Tuple(t) => t.iter().any(|v| matches!(v, Value::Literal(x) if x == lit)),
        _ => false,
    }
}

/// `os.path.join(a, b)`.
fn path_join(a: &str, b: &str) -> String {
    if b.starts_with('/') || a.is_empty() {
        b.to_string()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `os.path.exists(path)`.
fn path_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok()
}

/// `obj.attr` of an object that has no such attribute.
fn attribute_error(v: &Value, attr: &str) -> PyErr {
    PyErr::new(ExcKind::AttributeError, format!("'{}' object has no attribute '{}'", v.type_name(), attr))
}

/// `params["JBIG2Globals"]`.
fn getitem_str(container: &Value, key: &str) -> PyResult<Value> {
    match container {
        Value::Dict(d) => {
            d.borrow().get(key).cloned().ok_or_else(|| PyErr::new(ExcKind::KeyError, crate::pyrt::fmt::str_repr(key)))
        }
        Value::Stream(s) => s.getitem(key),
        Value::List(_) | Value::Tuple(_) => Err(PyErr::new(
            ExcKind::TypeError,
            format!("{} indices must be integers or slices, not str", container.type_name()),
        )),
        Value::Bytes(_) => Err(PyErr::new(ExcKind::TypeError, "byte indices must be integers or slices, not str")),
        Value::Str(_) => Err(PyErr::new(ExcKind::TypeError, "string indices must be integers, not 'str'")),
        _ => Err(PyErr::new(ExcKind::TypeError, format!("'{}' object is not subscriptable", container.type_name()))),
    }
}

/// Write image to a file
///
/// Supports various image types: JPEG, JBIG2 and bitmaps
pub struct ImageWriter {
    pub outdir: String,
}

impl ImageWriter {
    /// `ImageWriter(outdir)`: creates the directory if it does not exist.
    pub fn new(outdir: &str) -> PyResult<ImageWriter> {
        if !path_exists(outdir) {
            std::fs::create_dir_all(outdir).map_err(|e| os_error_path(&e, outdir))?;
        }
        Ok(ImageWriter { outdir: outdir.to_string() })
    }

    /// Save an LTImage to disk
    pub fn export_image(&self, image: &LTItem) -> PyResult<String> {
        let img = image.as_image().expect("export_image needs an LTImage");
        let (width, height) = img.srcsize.clone();

        let filters = img.stream.get_filters()?;

        let name = if filters.is_empty() {
            self._save_bytes(image)?
        } else if in_literals(&filters[filters.len() - 1].0, LITERALS_DCT_DECODE) {
            self._save_jpeg(image)?
        } else if in_literals(&filters[filters.len() - 1].0, LITERALS_JPX_DECODE) {
            self._save_jpeg2000(image)?
        } else if Self::_is_jbig2_iamge(image)? {
            self._save_jbig2(image)?
        } else if py_eq(&img.bits, &Value::Int(1)) {
            let bpl = py_floordiv(&py_add(&width, &Value::Int(7))?, &Value::Int(8))?;
            self._save_bmp(image, &width, &height, &bpl, &img.bits)?
        } else if py_eq(&img.bits, &Value::Int(8))
            && (colorspace_has(&img.colorspace, &literal_device_rgb())
                || colorspace_has(&img.colorspace, &literal_inline_device_rgb()))
        {
            let bpl = py_mul(&width, &Value::Int(3))?;
            let bits = py_mul(&img.bits, &Value::Int(3))?;
            self._save_bmp(image, &width, &height, &bpl, &bits)?
        } else if py_eq(&img.bits, &Value::Int(8))
            && (colorspace_has(&img.colorspace, &literal_device_gray())
                || colorspace_has(&img.colorspace, &literal_inline_device_gray()))
        {
            self._save_bmp(image, &width, &height, &width, &img.bits)?
        } else if filters.len() == 1 && in_literals(&filters[0].0, LITERALS_FLATE_DECODE) {
            self._save_bytes(image)?
        } else {
            self._save_raw(image)?
        };

        Ok(name)
    }

    fn stream(image: &LTItem) -> &Rc<PDFStream> {
        &image.as_image().expect("LTImage").stream
    }

    /// Save a JPEG encoded image
    fn _save_jpeg(&self, image: &LTItem) -> PyResult<String> {
        let data = Self::stream(image).get_data()?;

        let (name, path) = self._create_unique_image_name(image, ".jpg");
        let mut fp = OutFile::open(&path)?;
        let r = if colorspace_has(&image.as_image().unwrap().colorspace, &literal_device_cmyk()) {
            Err(import_error())
        } else {
            fp.write(&data);
            Ok(())
        };
        fp.close(r)?;

        Ok(name)
    }

    /// Save a JPEG 2000 encoded image
    fn _save_jpeg2000(&self, image: &LTItem) -> PyResult<String> {
        let _data = Self::stream(image).get_data()?;

        let (_name, path) = self._create_unique_image_name(image, ".jp2");
        let fp = OutFile::open(&path)?;
        fp.close(Err(import_error()))
    }

    /// Save a JBIG2 encoded image
    fn _save_jbig2(&self, image: &LTItem) -> PyResult<String> {
        let (name, path) = self._create_unique_image_name(image, ".jb2");
        let mut fp = OutFile::open(&path)?;
        let r = (|| -> PyResult<()> {
            let mut input_stream: Vec<u8> = Vec::new();

            let mut global_streams = Vec::new();
            let filters = Self::stream(image).get_filters()?;
            for (filter_name, params) in &filters {
                if in_literals(filter_name, LITERALS_JBIG2_DECODE) {
                    let g = getitem_str(params, "JBIG2Globals")?;
                    let Value::Ref(r) = &g else {
                        return Err(attribute_error(&g, "resolve"));
                    };
                    global_streams.push(r.resolve(&Value::None)?);
                }
            }

            if global_streams.len() > 1 {
                let msg = "There should never be more than one JBIG2Globals associated with a JBIG2 embedded image";
                return Err(PyErr::new(ExcKind::PDFValueError, msg));
            }
            if global_streams.len() == 1 {
                let Value::Stream(s) = &global_streams[0] else {
                    return Err(attribute_error(&global_streams[0], "get_data"));
                };
                let data = s.get_data()?;
                let mut end = data.len();
                while end > 0 && data[end - 1] == b'\n' {
                    end -= 1;
                }
                input_stream.extend_from_slice(&data[..end]);
            }
            input_stream.extend_from_slice(&Self::stream(image).get_data()?);
            let (out, err) = jbig2_convert(&input_stream, true);
            fp.write(&out);
            match err {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })();
        fp.close(r)?;
        Ok(name)
    }

    /// Save a BMP encoded image
    fn _save_bmp(
        &self,
        image: &LTItem,
        width: &Value,
        height: &Value,
        bytes_per_line: &Value,
        bits: &Value,
    ) -> PyResult<String> {
        let (name, path) = self._create_unique_image_name(image, ".bmp");
        let mut fp = OutFile::open(&path)?;
        let r = (|| -> PyResult<()> {
            let bmp = BMPWriter::new(&mut fp, bits, width, height)?;
            let data = Self::stream(image).get_data()?;
            let mut i = Value::Int(0);
            for y in 0..index_int(height, false)? {
                let end = py_add(&i, bytes_per_line)?;
                let line = slice(&data, index_int(&i, true)?, index_int(&end, true)?);
                bmp.write_line(&mut fp, y, line)?;
                i = py_add(&i, bytes_per_line)?;
            }
            Ok(())
        })();
        fp.close(r)?;
        Ok(name)
    }

    /// Save an image without encoding, just bytes
    fn _save_bytes(&self, image: &LTItem) -> PyResult<String> {
        let (_name, path) = self._create_unique_image_name(image, ".jpg");
        let img = image.as_image().unwrap();
        let (width, height) = img.srcsize.clone();
        let n = Value::Int(img.stream.get_data()?.len() as i64);
        let _channels =
            py_truediv(&py_truediv(&py_truediv(&n, &width)?, &height)?, &py_truediv(&img.bits, &Value::Int(8))?)?;
        let fp = OutFile::open(&path)?;
        // `from PIL import Image, ImageOps` fails without Pillow.
        fp.close(Err(import_error()))
    }

    /// Save an image with unknown encoding
    fn _save_raw(&self, image: &LTItem) -> PyResult<String> {
        let img = image.as_image().unwrap();
        let ext = format!(".{}.{}x{}.img", img.bits.py_str(), img.srcsize.0.py_str(), img.srcsize.1.py_str());
        let (name, path) = self._create_unique_image_name(image, &ext);

        let mut fp = OutFile::open(&path)?;
        let r = img.stream.get_data().map(|d| fp.write(&d));
        fp.close(r)?;
        Ok(name)
    }

    fn _is_jbig2_iamge(image: &LTItem) -> PyResult<bool> {
        let filters = Self::stream(image).get_filters()?;
        Ok(filters.iter().any(|(filter_name, _)| in_literals(filter_name, LITERALS_JBIG2_DECODE)))
    }

    fn _create_unique_image_name(&self, image: &LTItem, ext: &str) -> (String, String) {
        let img_name = &image.as_image().unwrap().name;
        let mut name = format!("{img_name}{ext}");
        let mut path = path_join(&self.outdir, &name);
        let mut img_index = 0;
        while path_exists(&path) {
            name = format!("{img_name}.{img_index}{ext}");
            path = path_join(&self.outdir, &name);
            img_index += 1;
        }
        (name, path)
    }
}
