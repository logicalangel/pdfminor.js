//! A Rust port of [pdfminer.six](https://github.com/pdfminer/pdfminer.six) that produces the same
//! output, byte for byte.
//!
//! Every module mirrors the Python module of the same name (`pdfminer/psparser.py` ->
//! [`psparser`]); `UPSTREAM.toml` records the upstream version each one was ported from, and
//! `docs/PORTING.md` the rules. [`pyrt`] holds the Python semantics the port depends on.

#![allow(clippy::too_many_arguments, clippy::type_complexity)]

pub mod pyrt;

pub mod _saslprep;
pub mod arcfour;
pub mod ascii85;
pub mod casting;
pub mod ccitt;
pub mod cmapdb;
pub mod converter;
pub mod data_structures;
pub mod dump;
pub mod encodingdb;
pub mod fontmetrics;
pub mod glyphlist;
pub mod high_level;
pub mod image;
pub mod jbig2;
pub mod latin_enc;
pub mod layout;
pub mod lzw;
pub mod pdfcolor;
pub mod pdfdevice;
pub mod pdfdocument;
pub mod pdfexceptions;
pub mod pdffont;
pub mod pdfinterp;
pub mod pdfpage;
pub mod pdfparser;
pub mod pdftypes;
pub mod psexceptions;
pub mod psparser;
pub mod runlength;
pub mod settings;
pub mod utils;

/// The pdfminer.six release this port follows.
pub const UPSTREAM_VERSION: &str = "20260107";
