//! Python exceptions.
//!
//! pdfminer recovers from broken PDFs by catching exceptions by class (`except PSEOF`,
//! `except (KeyError, ValueError)`, `except Exception`, ...), so the port must raise the same
//! class at the same place and catch with the same test. Every exception class pdfminer raises or
//! catches is an [`ExcKind`]; [`ExcKind::is`] answers `isinstance(exc, cls)` with the same class
//! hierarchy as Python (including the multiple inheritance in `pdfexceptions.py`).

use std::fmt;

/// An exception class. Python built-ins first, then pdfminer's own classes (named as in Python,
/// with the module that defines them).
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExcKind {
    // builtins
    Exception,
    ArithmeticError,
    AssertionError,
    AttributeError,
    EOFError,
    IndexError,
    KeyError,
    LookupError,
    MemoryError,
    NotImplementedError,
    OSError,
    OverflowError,
    RecursionError,
    RuntimeError,
    TypeError,
    UnicodeDecodeError,
    UnicodeEncodeError,
    UnicodeError,
    ValueError,
    ZeroDivisionError,
    /// `struct.error`
    StructError,
    /// `zlib.error`
    ZlibError,
    /// `binascii.Error` (a `ValueError`)
    BinasciiError,
    // psexceptions.py
    PSException,
    PSEOF,
    PSSyntaxError,
    PSTypeError,
    PSValueError,
    // pdfexceptions.py
    PDFException,
    PDFTypeError,
    PDFValueError,
    PDFObjectNotFound,
    PDFNotImplementedError,
    PDFKeyError,
    PDFEOFError,
    PDFIOError,
    // pdfparser.py
    PDFSyntaxError,
    // pdfdocument.py
    PDFNoValidXRef,
    PDFNoOutlines,
    PDFNoPageLabels,
    PDFDestinationNotFound,
    PDFEncryptionError,
    PDFPasswordIncorrect,
    PDFTextExtractionNotAllowed,
    // cmapdb.py
    CMapError,
    // lzw.py
    CorruptDataError,
    // pdffont.py
    PDFFontError,
    PDFUnicodeNotDefined,
    // pdfinterp.py
    PDFResourceError,
    PDFInterpreterError,
    // ccitt.py: CCITTG4Parser.InvalidData / ByteSkip / EOFB
    CCITTInvalidData,
    CCITTByteSkip,
    CCITTEOFB,
}

use ExcKind::*;

impl ExcKind {
    /// Direct base classes, in MRO order.
    pub fn bases(self) -> &'static [ExcKind] {
        match self {
            Exception => &[],
            ArithmeticError | AssertionError | AttributeError | EOFError | LookupError | MemoryError | OSError
            | RuntimeError | TypeError | ValueError | StructError | ZlibError => &[Exception],
            IndexError | KeyError => &[LookupError],
            NotImplementedError | RecursionError => &[RuntimeError],
            OverflowError | ZeroDivisionError => &[ArithmeticError],
            UnicodeError => &[ValueError],
            UnicodeDecodeError | UnicodeEncodeError => &[UnicodeError],
            BinasciiError => &[ValueError],
            PSException => &[Exception],
            PSEOF | PSSyntaxError | PSTypeError | PSValueError => &[PSException],
            PDFException => &[PSException],
            PDFTypeError => &[PDFException, TypeError],
            PDFValueError => &[PDFException, ValueError],
            PDFObjectNotFound => &[PDFException],
            PDFNotImplementedError => &[PDFException, NotImplementedError],
            PDFKeyError => &[PDFException, KeyError],
            PDFEOFError => &[PDFException, EOFError],
            PDFIOError => &[PDFException, OSError],
            PDFSyntaxError => &[PDFException],
            PDFNoValidXRef => &[PDFSyntaxError],
            PDFNoOutlines | PDFNoPageLabels | PDFDestinationNotFound | PDFEncryptionError => {
                &[PDFException]
            }
            PDFPasswordIncorrect | PDFTextExtractionNotAllowed => &[PDFEncryptionError],
            CMapError => &[PDFException],
            CorruptDataError => &[PDFException],
            PDFFontError => &[PDFException],
            PDFUnicodeNotDefined => &[PDFFontError],
            PDFResourceError | PDFInterpreterError => &[PDFException],
            CCITTInvalidData | CCITTByteSkip | CCITTEOFB => &[Exception],
        }
    }

    /// `issubclass(self, cls)`.
    pub fn is(self, cls: ExcKind) -> bool {
        self == cls || self.bases().iter().any(|b| b.is(cls))
    }

    /// The Python class name, as `type(e).__name__` prints it.
    pub fn name(self) -> &'static str {
        match self {
            StructError => "error",
            ZlibError => "error",
            BinasciiError => "Error",
            CCITTInvalidData => "InvalidData",
            CCITTByteSkip => "ByteSkip",
            CCITTEOFB => "EOFB",
            other => other.debug_name(),
        }
    }

    fn debug_name(self) -> &'static str {
        macro_rules! names {
            ($($k:ident),* $(,)?) => { match self { $($k => stringify!($k),)* } };
        }
        names!(
            Exception, ArithmeticError, AssertionError, AttributeError, EOFError, IndexError, KeyError,
            LookupError, MemoryError, NotImplementedError, OSError, OverflowError, RecursionError,
            RuntimeError, TypeError, UnicodeDecodeError, UnicodeEncodeError, UnicodeError,
            ValueError, ZeroDivisionError, StructError, ZlibError, BinasciiError, PSException,
            PSEOF, PSSyntaxError, PSTypeError, PSValueError, PDFException, PDFTypeError,
            PDFValueError, PDFObjectNotFound, PDFNotImplementedError, PDFKeyError, PDFEOFError,
            PDFIOError, PDFSyntaxError, PDFNoValidXRef, PDFNoOutlines, PDFNoPageLabels,
            PDFDestinationNotFound, PDFEncryptionError, PDFPasswordIncorrect,
            PDFTextExtractionNotAllowed, CMapError, CorruptDataError, PDFFontError,
            PDFUnicodeNotDefined, PDFResourceError, PDFInterpreterError, CCITTInvalidData,
            CCITTByteSkip, CCITTEOFB,
        )
    }
}

/// A raised exception: its class and `str(e)`.
#[derive(Clone, Debug, PartialEq)]
pub struct PyErr {
    pub kind: ExcKind,
    pub msg: String,
}

impl PyErr {
    pub fn new(kind: ExcKind, msg: impl Into<String>) -> Self {
        PyErr { kind, msg: msg.into() }
    }

    /// `isinstance(e, cls)`.
    pub fn is(&self, cls: ExcKind) -> bool {
        self.kind.is(cls)
    }

    /// `isinstance(e, (cls1, cls2, ...))`.
    pub fn is_any(&self, classes: &[ExcKind]) -> bool {
        classes.iter().any(|c| self.kind.is(*c))
    }
}

impl fmt::Display for PyErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.msg.is_empty() {
            write!(f, "{}", self.kind.name())
        } else {
            write!(f, "{}: {}", self.kind.name(), self.msg)
        }
    }
}

impl std::error::Error for PyErr {}

pub type PyResult<T> = Result<T, PyErr>;

/// `raise Kind(msg)`: `return Err(raise!(KeyError, "..."))` or `Err(raise!(...))?`.
#[macro_export]
macro_rules! raise {
    ($kind:ident) => {
        $crate::pyrt::error::PyErr::new($crate::pyrt::error::ExcKind::$kind, "")
    };
    ($kind:ident, $($arg:tt)+) => {
        $crate::pyrt::error::PyErr::new($crate::pyrt::error::ExcKind::$kind, format!($($arg)+))
    };
}

/// `try: ... except (A, B): ...` as a match on the result: `catch(r, &[A, B])` turns a caught
/// exception into `Ok(None)` and lets any other exception through.
pub fn catch<T>(r: PyResult<T>, classes: &[ExcKind]) -> PyResult<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.is_any(classes) => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchy_matches_python() {
        assert!(PDFKeyError.is(KeyError));
        assert!(PDFKeyError.is(LookupError));
        assert!(PDFKeyError.is(PSException));
        assert!(PDFTypeError.is(TypeError));
        assert!(PDFNoValidXRef.is(PDFSyntaxError));
        assert!(PDFNoValidXRef.is(PSException));
        assert!(UnicodeDecodeError.is(ValueError));
        assert!(!PSEOF.is(PDFException));
        assert!(PDFPasswordIncorrect.is(PDFEncryptionError));
        assert!(BinasciiError.is(ValueError));
    }
}
