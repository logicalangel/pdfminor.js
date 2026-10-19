//! `pdfminer-rs`: pdfminer.six's command line tools.
//!
//! - `pdfminer-rs pdf2txt ...`: `tools/pdf2txt.py`
//! - `pdfminer-rs dumppdf ...`: `tools/dumppdf.py`
//! - `pdfminer-rs dump FILE [--laparams default|none|JSON] [--password PW]`: the canonical layout
//!   dump of `tools/oracle.py layout` (see `pdfminer::dump`).
//!
//! An exception that escapes ends the program like Python's: what was written to stdout stays,
//! a traceback goes to stderr, and the exit code is 1. The traceback has no Python frames; its
//! last line is `Class: message` as Python's.

mod argparse;
mod dump;
mod dumppdf;
mod pdf2txt;

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use pdfminer::pyrt::error::PyErr;

/// The process's stdout, buffered, shared by Python's `sys.stdout` and `sys.stdout.buffer`.
#[derive(Clone)]
pub struct Stdout(Rc<RefCell<std::io::BufWriter<std::io::Stdout>>>);

impl Default for Stdout {
    fn default() -> Self {
        Self::new()
    }
}

impl Stdout {
    pub fn new() -> Self {
        Stdout(Rc::new(RefCell::new(std::io::BufWriter::with_capacity(1 << 16, std::io::stdout()))))
    }
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

/// What Python prints for an uncaught exception (without the frames).
pub fn traceback(e: &PyErr) -> String {
    let name = e.kind.qualified_name();
    let last = if e.msg.is_empty() { name } else { format!("{name}: {}", e.msg) };
    format!("Traceback (most recent call last):\n  File \"<pdfminer-rs>\", line 0, in <module>\n{last}\n")
}

/// Ends the program after an uncaught exception: flush stdout, traceback, exit 1.
pub fn die(stdout: &mut Stdout, e: &PyErr) -> ! {
    let _ = stdout.flush();
    let _ = std::io::stderr().write_all(traceback(e).as_bytes());
    std::process::exit(1);
}

fn usage() -> ! {
    eprintln!("usage: pdfminer-rs {{pdf2txt|dumppdf|dump}} ...");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    let Some(cmd) = args.first() else { usage() };
    let rest = &args[1..];
    let code = match cmd.as_str() {
        "pdf2txt" => pdf2txt::main(rest),
        "dumppdf" => dumppdf::main(rest),
        "dump" => dump::main(rest),
        _ => usage(),
    };
    std::process::exit(code);
}
