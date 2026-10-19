//! Python's text formatting of numbers, bytes and strings.
//!
//! Output that pdfminer writes with f-strings or `%` (`f"{x:.3f}"`, `"%d"`, `repr(x)`) must be
//! produced with these functions, never with Rust's `{}` / `{:?}`, whose float output differs
//! (`1` vs `1.0`, `1e-5` vs `1e-05`, ...).
//!
//! Rust's fixed-precision formatting (`{:.3}`) rounds the exact binary value half-to-even, like
//! Python's `.3f` (checked on 200k values, ties and -0.0 included), so [`fixed`] uses it.

use super::ctype::py_isprintable;
use super::pystr::PyStr;

/// `f"{x:.{prec}f}"` / `"%.{prec}f" % x`.
pub fn fixed(x: f64, prec: usize) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    format!("{:.*}", prec, x)
}

/// `repr(x)` (== `str(x)`) of a Python float: the shortest round-tripping digits, in fixed
/// notation when the decimal exponent is in -4..16, else `d.ddde+XX`.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Rust's `{:e}` prints the shortest round-tripping digits: "-1.2345e-5".
    let e = format!("{:e}", x);
    let (mant, exp) = e.split_once('e').expect("exponent");
    let exp: i32 = exp.parse().expect("exponent");
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mant),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    // Python: value = 0.DIGITS * 10**decpt
    let decpt = exp + 1;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if -4 < decpt && decpt <= 16 {
        if decpt <= 0 {
            out.push_str("0.");
            for _ in 0..(-decpt) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let d = decpt as usize;
            if digits.len() <= d {
                out.push_str(&digits);
                for _ in digits.len()..d {
                    out.push('0');
                }
                out.push_str(".0");
            } else {
                out.push_str(&digits[..d]);
                out.push('.');
                out.push_str(&digits[d..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", e.abs()));
    }
    out
}

/// `f"{x:g}"` / `"%g" % x` (precision 6).
pub fn general(x: f64) -> String {
    general_prec(x, 6)
}

/// `"%.{prec}g" % x`.
pub fn general_prec(x: f64, prec: usize) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let p = prec.max(1);
    if x == 0.0 {
        return if x.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let e = format!("{:.*e}", p - 1, x);
    let (_, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if exp < -4 || exp >= p as i32 {
        let (mant, _) = e.split_once('e').unwrap();
        let mant = strip_trailing_zeros(mant);
        format!("{}e{}{:02}", mant, if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        let s = format!("{:.*}", (p as i32 - 1 - exp).max(0) as usize, x);
        strip_trailing_zeros(&s).to_string()
    }
}

fn strip_trailing_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// `repr(b)` of a Python `bytes`.
pub fn bytes_repr(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') { '"' } else { '\'' };
    let mut out = String::with_capacity(b.len() + 3);
    out.push('b');
    out.push(quote);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            _ if c as char == quote => {
                out.push('\\');
                out.push(quote);
            }
            0x20..=0x7E => out.push(c as char),
            _ => out.push_str(&format!("\\x{:02x}", c)),
        }
    }
    out.push(quote);
    out
}

/// `repr(s)` of a Python `str`, given as code points.
pub fn str_repr_cps(cps: impl Iterator<Item = u32> + Clone) -> String {
    let has_single = cps.clone().any(|c| c == '\'' as u32);
    let has_double = cps.clone().any(|c| c == '"' as u32);
    let quote = if has_single && !has_double { '"' } else { '\'' };
    let mut out = String::new();
    out.push(quote);
    for cp in cps {
        match cp {
            0x5C => out.push_str("\\\\"),
            0x09 => out.push_str("\\t"),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            _ if cp == quote as u32 => {
                out.push('\\');
                out.push(quote);
            }
            _ if cp < 0x20 || cp == 0x7F => out.push_str(&format!("\\x{:02x}", cp)),
            _ if cp < 0x7F => out.push(cp as u8 as char),
            _ if py_isprintable(cp) => out.push(char::from_u32(cp).expect("printable is a char")),
            _ if cp <= 0xFF => out.push_str(&format!("\\x{:02x}", cp)),
            _ if cp <= 0xFFFF => out.push_str(&format!("\\u{:04x}", cp)),
            _ => out.push_str(&format!("\\U{:08x}", cp)),
        }
    }
    out.push(quote);
    out
}

/// `repr(s)` of a Python `str`.
pub fn str_repr(s: &str) -> String {
    str_repr_cps(s.chars().map(|c| c as u32))
}

/// `repr(s)` of a [`PyStr`].
pub fn pystr_repr(s: &PyStr) -> String {
    str_repr_cps(s.code_points())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (1.0, "1.0"),
            (-1.5, "-1.5"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (123456789012345678.0, "1.2345678901234568e+17"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-7, "1.5e-07"),
            (-0.0, "-0.0"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (100.0, "100.0"),
            (12.5, "12.5"),
            (0.3333333333333333, "0.3333333333333333"),
        ];
        for (x, s) in cases {
            assert_eq!(float_repr(x), s);
        }
    }

    #[test]
    fn general_matches_python() {
        assert_eq!(general(1.0), "1");
        assert_eq!(general(0.0001), "0.0001");
        assert_eq!(general(0.00001), "1e-05");
        assert_eq!(general(123456.0), "123456");
        assert_eq!(general(1234567.0), "1.23457e+06");
        assert_eq!(general(1.5), "1.5");
    }

    #[test]
    fn bytes_repr_matches_python() {
        assert_eq!(bytes_repr(b"abc"), "b'abc'");
        assert_eq!(bytes_repr(b"a'b"), "b\"a'b\"");
        assert_eq!(bytes_repr(b"a'b\""), "b'a\\'b\"'");
        assert_eq!(bytes_repr(b"\x00\xff\n"), "b'\\x00\\xff\\n'");
    }

    #[test]
    fn str_repr_matches_python() {
        assert_eq!(str_repr("abc"), "'abc'");
        assert_eq!(str_repr("é\u{a0}"), "'é\\xa0'");
        assert_eq!(str_repr_cps([0xD800u32].into_iter()), "'\\ud800'");
    }
}
