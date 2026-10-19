//! Port of `pdfminer/jbig2.py`.
//!
//! Python keeps a segment in a `dict`; here it is [`JBIG2Segment`]. The reader only returns
//! complete segments (every `SEG_STRUCT` field present), and the writer's own end-of-page and
//! end-of-file segments are complete too, so those fields are plain values. What can be missing
//! stays an `Option`: `raw_data` (only set when `data_length` is non-zero; the writer's
//! `segment["raw_data"]` then raises `KeyError`) and `flags["page_assoc_long"]` (absent in the
//! writer's own segments).
//!
//! `struct.pack`/`struct.unpack` errors are `ExcKind::StructError` with CPython's messages.

use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::fmt::str_repr;
use crate::pyrt::io::BytesIO;

// segment structure base
pub const SEG_STRUCT: [(&str, &str); 5] = [
    (">L", "number"),
    (">B", "flags"),
    (">B", "retention_flags"),
    (">B", "page_assoc"),
    (">L", "data_length"),
];

// segment header literals
pub const HEADER_FLAG_DEFERRED: i64 = 0b10000000;
pub const HEADER_FLAG_PAGE_ASSOC_LONG: i64 = 0b01000000;

pub const SEG_TYPE_MASK: i64 = 0b00111111;

pub const REF_COUNT_SHORT_MASK: i64 = 0b11100000;
pub const REF_COUNT_LONG_MASK: i64 = 0x1FFFFFFF;
pub const REF_COUNT_LONG: i64 = 7;

pub const DATA_LEN_UNKNOWN: i64 = 0xFFFFFFFF;

// segment types
pub const SEG_TYPE_IMMEDIATE_GEN_REGION: i64 = 38;
pub const SEG_TYPE_END_OF_PAGE: i64 = 49;
pub const SEG_TYPE_END_OF_FILE: i64 = 51;

// file literals
pub const FILE_HEADER_ID: &[u8] = b"\x97\x4a\x42\x32\x0d\x0a\x1a\x0a";
pub const FILE_HEAD_FLAG_SEQUENTIAL: i64 = 0b00000001;

pub fn bit_set(bit_pos: i64, value: i64) -> bool {
    ((value >> bit_pos) & 1) != 0
}

pub fn check_flag(flag: i64, value: i64) -> bool {
    (flag & value) != 0
}

pub fn masked_value(mask: i64, value: i64) -> PyResult<i64> {
    for bit_pos in 0..31 {
        if bit_set(bit_pos, mask) {
            return Ok((value & mask) >> bit_pos);
        }
    }

    Err(PyErr::new(ExcKind::PDFValueError, "Invalid mask or value"))
}

pub fn mask_value(mask: i64, value: i64) -> PyResult<i64> {
    for bit_pos in 0..31 {
        if bit_set(bit_pos, mask) {
            return Ok((value & (mask >> bit_pos)) << bit_pos);
        }
    }

    Err(PyErr::new(ExcKind::PDFValueError, "Invalid mask or value"))
}

/// `struct.calcsize(fmt)` for the big-endian one-item formats used here.
fn calcsize(format: &str) -> usize {
    match format {
        ">B" => 1,
        ">I" | ">L" => 4,
        _ => unreachable!("format {format}"),
    }
}

pub fn unpack_int(format: &str, buffer: &[u8]) -> PyResult<i64> {
    assert!(matches!(format, ">B" | ">I" | ">L"));
    let size = calcsize(format);
    if buffer.len() != size {
        return Err(PyErr::new(ExcKind::StructError, format!("unpack requires a buffer of {size} bytes")));
    }
    Ok(buffer.iter().fold(0i64, |acc, &b| (acc << 8) | b as i64))
}

/// `struct.pack(">" + codes, *values)` for the codes `B`, `I`, `L` (standard sizes 1, 4, 4).
fn pack(codes: &str, values: &[i64]) -> PyResult<Vec<u8>> {
    debug_assert_eq!(codes.len(), values.len());
    let mut out = Vec::with_capacity(values.len() * 4);
    for (code, &v) in codes.chars().zip(values) {
        let (size, max) = match code {
            'B' => (1usize, 255i64),
            'I' | 'L' => (4, 4294967295),
            _ => unreachable!("code {code}"),
        };
        if !(0..=max).contains(&v) {
            return Err(PyErr::new(
                ExcKind::StructError,
                format!("'{code}' format requires 0 <= number <= {max}"),
            ));
        }
        out.extend_from_slice(&(v as u64).to_be_bytes()[8 - size..]);
    }
    Ok(out)
}

/// `JBIG2SegmentFlags`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JBIG2SegmentFlags {
    pub deferred: bool,
    /// Absent in the writer's own segments.
    pub page_assoc_long: Option<bool>,
    pub type_: i64,
}

/// `JBIG2RetentionFlags`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JBIG2RetentionFlags {
    pub ref_count: i64,
    pub retain_segments: Vec<bool>,
    pub ref_segments: Vec<i64>,
}

/// `JBIG2Segment`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JBIG2Segment {
    pub number: i64,
    pub flags: JBIG2SegmentFlags,
    pub retention_flags: JBIG2RetentionFlags,
    pub page_assoc: i64,
    pub data_length: i64,
    pub raw_data: Option<Vec<u8>>,
}

/// A segment being read (the dict before all fields are set).
#[derive(Default)]
struct PartialSegment {
    number: Option<i64>,
    flags: Option<JBIG2SegmentFlags>,
    retention_flags: Option<JBIG2RetentionFlags>,
    page_assoc: Option<i64>,
    data_length: Option<i64>,
    raw_data: Option<Vec<u8>>,
    _error: bool,
}

/// Read segments from a JBIG2 byte stream
pub struct JBIG2StreamReader<'a> {
    pub stream: &'a mut BytesIO,
}

impl<'a> JBIG2StreamReader<'a> {
    pub fn new(stream: &'a mut BytesIO) -> Self {
        JBIG2StreamReader { stream }
    }

    pub fn get_segments(&mut self) -> PyResult<Vec<JBIG2Segment>> {
        let mut segments: Vec<JBIG2Segment> = vec![];
        while !self.is_eof()? {
            let mut segment = PartialSegment::default();
            for (field_format, name) in SEG_STRUCT {
                let field_len = calcsize(field_format);
                let field = self.stream.read(field_len as i64).to_vec();
                if field.len() < field_len {
                    segment._error = true;
                    break;
                }
                let value = unpack_int(field_format, &field)?;
                match name {
                    "number" => segment.number = Some(value),
                    "flags" => segment.flags = Some(self.parse_flags(&segment, value, &field)?),
                    "retention_flags" => {
                        segment.retention_flags = Some(self.parse_retention_flags(&segment, value, field)?)
                    }
                    "page_assoc" => segment.page_assoc = Some(self.parse_page_assoc(&segment, value, field)?),
                    "data_length" => {
                        let v = self.parse_data_length(&mut segment, value, &field)?;
                        segment.data_length = Some(v);
                    }
                    _ => unreachable!(),
                }
            }

            if !segment._error {
                segments.push(JBIG2Segment {
                    number: segment.number.expect("number"),
                    flags: segment.flags.expect("flags"),
                    retention_flags: segment.retention_flags.expect("retention_flags"),
                    page_assoc: segment.page_assoc.expect("page_assoc"),
                    data_length: segment.data_length.expect("data_length"),
                    raw_data: segment.raw_data,
                });
            }
        }
        Ok(segments)
    }

    pub fn is_eof(&mut self) -> PyResult<bool> {
        if self.stream.read(1).is_empty() {
            Ok(true)
        } else {
            // self.stream.seek(-1, os.SEEK_CUR)
            let pos = self.stream.tell() - 1;
            self.stream.seek(pos)?;
            Ok(false)
        }
    }

    fn parse_flags(&mut self, _segment: &PartialSegment, flags: i64, _field: &[u8]) -> PyResult<JBIG2SegmentFlags> {
        Ok(JBIG2SegmentFlags {
            deferred: check_flag(HEADER_FLAG_DEFERRED, flags),
            page_assoc_long: Some(check_flag(HEADER_FLAG_PAGE_ASSOC_LONG, flags)),
            type_: masked_value(SEG_TYPE_MASK, flags)?,
        })
    }

    fn parse_retention_flags(
        &mut self,
        segment: &PartialSegment,
        flags: i64,
        mut field: Vec<u8>,
    ) -> PyResult<JBIG2RetentionFlags> {
        let mut ref_count = masked_value(REF_COUNT_SHORT_MASK, flags)?;
        let mut retain_segments = vec![];
        let mut ref_segments = vec![];

        if ref_count < REF_COUNT_LONG {
            for bit_pos in 0..5 {
                retain_segments.push(bit_set(bit_pos, flags));
            }
        } else {
            field.extend_from_slice(self.stream.read(3));
            ref_count = unpack_int(">L", &field)?;
            ref_count = masked_value(REF_COUNT_LONG_MASK, ref_count)?;
            // math.ceil((ref_count + 1) / 8)
            let ret_bytes_count = (ref_count + 1 + 7).div_euclid(8);
            for _ret_byte_index in 0..ret_bytes_count {
                let ret_byte = unpack_int(">B", self.stream.read(1))?;
                for bit_pos in 0..7 {
                    retain_segments.push(bit_set(bit_pos, ret_byte));
                }
            }
        }

        let seg_num = segment.number.expect("number is read first");
        let ref_format = if seg_num <= 256 {
            ">B"
        } else if seg_num <= 65536 {
            ">I"
        } else {
            ">L"
        };

        let ref_size = calcsize(ref_format);

        for _ref_index in 0..ref_count {
            let ref_data = self.stream.read(ref_size as i64);
            let r = unpack_int(ref_format, ref_data)?;
            ref_segments.push(r);
        }

        Ok(JBIG2RetentionFlags { ref_count, retain_segments, ref_segments })
    }

    fn parse_page_assoc(&mut self, segment: &PartialSegment, mut page: i64, mut field: Vec<u8>) -> PyResult<i64> {
        if segment.flags.as_ref().expect("flags").page_assoc_long == Some(true) {
            field.extend_from_slice(self.stream.read(3));
            page = unpack_int(">L", &field)?;
        }
        Ok(page)
    }

    fn parse_data_length(&mut self, segment: &mut PartialSegment, length: i64, _field: &[u8]) -> PyResult<i64> {
        if length != 0 {
            if segment.flags.as_ref().expect("flags").type_ == SEG_TYPE_IMMEDIATE_GEN_REGION
                && length == DATA_LEN_UNKNOWN
            {
                return Err(PyErr::new(
                    ExcKind::NotImplementedError,
                    "Working with unknown segment length is not implemented yet",
                ));
            } else {
                segment.raw_data = Some(self.stream.read(length).to_vec());
            }
        }

        Ok(length)
    }
}

/// Write JBIG2 segments to a file in JBIG2 format
///
/// The output goes to `stream`; on an exception, what was written before it stays there (as in
/// the file Python writes to).
pub struct JBIG2StreamWriter<'a> {
    pub stream: &'a mut Vec<u8>,
}

impl<'a> JBIG2StreamWriter<'a> {
    /// `EMPTY_RETENTION_FLAGS`
    pub fn empty_retention_flags() -> JBIG2RetentionFlags {
        JBIG2RetentionFlags { ref_count: 0, ref_segments: vec![], retain_segments: vec![] }
    }

    pub fn new(stream: &'a mut Vec<u8>) -> Self {
        JBIG2StreamWriter { stream }
    }

    pub fn write_segments(&mut self, segments: &[JBIG2Segment], fix_last_page: bool) -> PyResult<i64> {
        let mut data_len: i64 = 0;
        let mut current_page: Option<i64> = None;
        let mut seg_num: Option<i64> = None;

        for segment in segments {
            let data = self.encode_segment(segment)?;
            self.stream.extend_from_slice(&data);
            data_len += data.len() as i64;

            seg_num = Some(segment.number);

            if fix_last_page {
                let seg_page = segment.page_assoc;

                if segment.flags.type_ == SEG_TYPE_END_OF_PAGE {
                    current_page = None;
                } else if seg_page != 0 {
                    current_page = Some(seg_page);
                }
            }
        }

        if let (true, Some(current_page), Some(seg_num)) = (fix_last_page, current_page, seg_num)
            && current_page != 0
        {
            let segment = self.get_eop_segment(seg_num + 1, current_page);
            let data = self.encode_segment(&segment)?;
            self.stream.extend_from_slice(&data);
            data_len += data.len() as i64;
        }

        Ok(data_len)
    }

    pub fn write_file(&mut self, segments: &[JBIG2Segment], fix_last_page: bool) -> PyResult<i64> {
        let mut header = FILE_HEADER_ID.to_vec();
        let header_flags = FILE_HEAD_FLAG_SEQUENTIAL;
        header.extend(pack("B", &[header_flags])?);
        // The embedded JBIG2 files in a PDF always
        // only have one page
        let number_of_pages = pack("L", &[1])?;
        header.extend(number_of_pages);
        self.stream.extend_from_slice(&header);
        let mut data_len = header.len() as i64;

        data_len += self.write_segments(segments, fix_last_page)?;

        let mut seg_num = 0;
        for segment in segments {
            seg_num = segment.number;
        }

        let seg_num_offset = if fix_last_page { 2 } else { 1 };
        let eof_segment = self.get_eof_segment(seg_num + seg_num_offset);
        let data = self.encode_segment(&eof_segment)?;

        self.stream.extend_from_slice(&data);
        data_len += data.len() as i64;

        Ok(data_len)
    }

    pub fn encode_segment(&self, segment: &JBIG2Segment) -> PyResult<Vec<u8>> {
        let mut data = vec![];
        for (_field_format, name) in SEG_STRUCT {
            let field = match name {
                "number" => pack("L", &[segment.number])?,
                "flags" => self.encode_flags(&segment.flags, segment)?,
                "retention_flags" => self.encode_retention_flags(&segment.retention_flags, segment)?,
                "page_assoc" => pack("B", &[segment.page_assoc])?,
                "data_length" => self.encode_data_length(segment.data_length, segment)?,
                _ => unreachable!(),
            };
            data.extend(field);
        }
        Ok(data)
    }

    pub fn encode_flags(&self, value: &JBIG2SegmentFlags, _segment: &JBIG2Segment) -> PyResult<Vec<u8>> {
        let mut flags = 0;
        if value.deferred {
            flags |= HEADER_FLAG_DEFERRED;
        }

        if let Some(page_assoc_long) = value.page_assoc_long {
            flags |= if page_assoc_long { HEADER_FLAG_PAGE_ASSOC_LONG } else { flags };
        } else {
            // segment.get("page", 0) > 255: segments never have a "page" key, so this is 0.
            flags |= flags;
        }

        flags |= mask_value(SEG_TYPE_MASK, value.type_)?;

        pack("B", &[flags])
    }

    pub fn encode_retention_flags(&self, value: &JBIG2RetentionFlags, segment: &JBIG2Segment) -> PyResult<Vec<u8>> {
        let mut flags: Vec<i64> = vec![];
        let mut flags_format = String::from("B");
        let ref_count = value.ref_count;
        let retain_segments = &value.retain_segments;

        if ref_count <= 4 {
            let mut flags_byte = mask_value(REF_COUNT_SHORT_MASK, ref_count)?;
            for (ref_index, &ref_retain) in retain_segments.iter().enumerate() {
                if ref_retain {
                    // 1 << ref_index: a Python int, so it does not wrap
                    if ref_index >= 62 {
                        return Err(PyErr::new(ExcKind::StructError, "'B' format requires 0 <= number <= 255"));
                    }
                    flags_byte |= 1 << ref_index;
                }
            }
            flags.push(flags_byte);
        } else {
            // math.ceil((ref_count + 1) / 8)
            let bytes_count = (ref_count + 1 + 7).div_euclid(8);
            flags_format = "L".to_string() + &"B".repeat(bytes_count.max(0) as usize);
            let flags_dword = mask_value(REF_COUNT_SHORT_MASK, REF_COUNT_LONG)? << 24;
            flags.push(flags_dword);

            for byte_index in 0..bytes_count {
                let mut ret_byte = 0;
                let lo = ((byte_index * 8) as usize).min(retain_segments.len());
                let hi = ((byte_index * 8 + 8) as usize).min(retain_segments.len());
                let ret_part = &retain_segments[lo..hi];
                for (bit_pos, &ret_seg) in ret_part.iter().enumerate() {
                    ret_byte |= if ret_seg { 1 << bit_pos } else { ret_byte };
                }

                flags.push(ret_byte);
            }
        }

        let ref_segments = &value.ref_segments;

        let seg_num = segment.number;
        let ref_format = if seg_num <= 256 {
            "B"
        } else if seg_num <= 65536 {
            "I"
        } else {
            "L"
        };

        for &r in ref_segments {
            flags_format += ref_format;
            flags.push(r);
        }

        pack(&flags_format, &flags)
    }

    pub fn encode_data_length(&self, value: i64, segment: &JBIG2Segment) -> PyResult<Vec<u8>> {
        let mut data = pack("L", &[value])?;
        match &segment.raw_data {
            Some(raw) => data.extend_from_slice(raw),
            None => return Err(PyErr::new(ExcKind::KeyError, str_repr("raw_data"))),
        }
        Ok(data)
    }

    pub fn get_eop_segment(&self, seg_number: i64, page_number: i64) -> JBIG2Segment {
        JBIG2Segment {
            data_length: 0,
            flags: JBIG2SegmentFlags { deferred: false, page_assoc_long: None, type_: SEG_TYPE_END_OF_PAGE },
            number: seg_number,
            page_assoc: page_number,
            raw_data: Some(vec![]),
            retention_flags: Self::empty_retention_flags(),
        }
    }

    pub fn get_eof_segment(&self, seg_number: i64) -> JBIG2Segment {
        JBIG2Segment {
            data_length: 0,
            flags: JBIG2SegmentFlags { deferred: false, page_assoc_long: None, type_: SEG_TYPE_END_OF_FILE },
            number: seg_number,
            page_assoc: 0,
            raw_data: Some(vec![]),
            retention_flags: Self::empty_retention_flags(),
        }
    }
}

/// `JBIG2StreamReader(BytesIO(data)).get_segments()` then
/// `JBIG2StreamWriter(out).write_file(segments)`: what `ImageWriter._save_jbig2` does with the
/// stream data. Returns the bytes written, and the exception if one was raised (the bytes are
/// then what Python had written to the file before it).
pub fn jbig2_convert(data: &[u8], fix_last_page: bool) -> (Vec<u8>, Option<PyErr>) {
    let mut input = BytesIO::from_vec(data.to_vec());
    let segments = match JBIG2StreamReader::new(&mut input).get_segments() {
        Ok(s) => s,
        Err(e) => return (vec![], Some(e)),
    };
    let mut out = vec![];
    let r = JBIG2StreamWriter::new(&mut out).write_file(&segments, fix_last_page);
    (out, r.err())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_minimal() {
        // One page-information segment (type 48) of page 1 with 3 data bytes.
        let mut data = vec![0, 0, 0, 0, 48, 0, 1, 0, 0, 0, 3, 1, 2, 3];
        data.extend_from_slice(&[0, 0, 0, 1, 50, 0, 1, 0, 0, 0, 0]); // end of stripe, no data
        let (out, err) = jbig2_convert(&data, true);
        // The second segment has no raw_data: KeyError, after the header and first segment.
        let err = err.unwrap();
        assert!(err.is(ExcKind::KeyError));
        assert_eq!(err.msg, "'raw_data'");
        assert_eq!(&out[..8], FILE_HEADER_ID);
        assert_eq!(out.len(), 13 + 14);
    }

    #[test]
    fn pack_errors() {
        assert_eq!(pack("B", &[256]).unwrap_err().msg, "'B' format requires 0 <= number <= 255");
        assert_eq!(unpack_int(">L", b"ab").unwrap_err().msg, "unpack requires a buffer of 4 bytes");
    }
}
