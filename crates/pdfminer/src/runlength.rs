//! Port of `pdfminer/runlength.py`.
//
// RunLength decoder (Adobe version) implementation based on PDF Reference
// version 1.4 section 3.3.4.
//
//  * public domain *
//

use crate::pyrt::error::{ExcKind, PyErr, PyResult};

/// RunLength decoder (Adobe version) implementation based on PDF Reference
/// version 1.4 section 3.3.4:
///     The RunLengthDecode filter decodes data that has been encoded in a
///     simple byte-oriented format based on run length. The encoded data
///     is a sequence of runs, where each run consists of a length byte
///     followed by 1 to 128 bytes of data. If the length byte is in the
///     range 0 to 127, the following length + 1 (1 to 128) bytes are
///     copied literally during decompression. If length is in the range
///     129 to 255, the following single byte is to be copied 257 - length
///     (2 to 128) times during decompression. A length value of 128
///     denotes EOD.
///
/// Truncated input raises what Python raises: a literal run is read by a generator expression,
/// so a missing byte there is `RuntimeError("generator raised StopIteration")`; a missing
/// repeated byte is a bare `StopIteration` from `next(data_iter)`.
pub fn rldecode(data: &[u8]) -> PyResult<Vec<u8>> {
    let mut decoded_array: Vec<u8> = Vec::new();
    let mut data_iter = data.iter().copied();

    loop {
        let length = data_iter.next().unwrap_or(128);
        if length == 128 {
            break;
        }

        if length < 128 {
            // decoded_array.extend(next(data_iter) for _ in range(length + 1))
            for _ in 0..(length as usize + 1) {
                match data_iter.next() {
                    Some(b) => decoded_array.push(b),
                    None => {
                        return Err(PyErr::new(ExcKind::RuntimeError, "generator raised StopIteration"));
                    }
                }
            }
        }

        if length > 128 {
            // run = [next(data_iter)] * (257 - length)
            let Some(b) = data_iter.next() else {
                return Err(PyErr::new(ExcKind::StopIteration, ""));
            };
            decoded_array.extend(std::iter::repeat_n(b, 257 - length as usize));
        }
    }
    Ok(decoded_array)
}

#[cfg(test)]
mod tests {
    use super::*;

    // tests/test_pdfminer_crypto.py: TestRunlength

    #[test]
    fn test_rldecode() {
        assert_eq!(rldecode(b"\x05123456\xfa7\x04abcde\x80junk").unwrap(), b"1234567777777abcde");
    }

    #[test]
    fn truncated() {
        assert!(rldecode(b"\x05ab").unwrap_err().is(ExcKind::RuntimeError));
        assert!(rldecode(b"\xfa").unwrap_err().is(ExcKind::StopIteration));
        assert_eq!(rldecode(b"").unwrap(), b"");
    }
}
