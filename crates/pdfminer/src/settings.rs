//! Port of `pdfminer/settings.py`.

/// `STRICT`: raise on malformed input instead of recovering. pdfminer's default (False) is the
/// only mode this port supports; the strict branches are ported for completeness.
pub const STRICT: bool = false;
