//! Port of `pdfminer/ccitt.py`.
//
// CCITT Fax decoder
//
// Bugs: uncompressed mode untested.
//
// cf.
//  ITU-T Recommendation T.4
//    "Standardization of Group 3 facsimile terminals
//    for document transmission"
//  ITU-T Recommendation T.6
//    "FACSIMILE CODING SCHEMES AND CODING CONTROL FUNCTIONS
//    FOR GROUP 4 FACSIMILE APPARATUS"
//
// The bit trees (`BitParserState`, nested 2-item lists) are an arena of nodes; a parser state is
// a node index. `_curline`/`_refline` (`array.array("b")`) are `Vec<i8>`, indexed with Python's
// rules (negative indices count from the end; out of range raises `IndexError`).
//
// `CCITTG4Parser.CCITTException(PDFException)` and its subclasses `EOFB`, `InvalidData`,
// `ByteSkip` are `ExcKind::CCITTEOFB`, `CCITTInvalidData`, `CCITTByteSkip`.

use std::sync::OnceLock;

use crate::pdftypes::py_get_str;
use crate::pyrt::error::{ExcKind, PyErr, PyResult};
use crate::pyrt::value::{Value, py_eq};

/// A leaf of a bit tree: the `int | str` values passed to `BitParser.add`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sym {
    Int(i64),
    Str(&'static str),
}

impl Sym {
    /// `str(v)`.
    fn py_str(self) -> String {
        match self {
            Sym::Int(i) => i.to_string(),
            Sym::Str(s) => s.to_string(),
        }
    }
}

/// One item of a `BitParserState` list: `None`, a nested list, or a value.
#[derive(Clone, Copy, Debug)]
enum Slot {
    None,
    Node(u32),
    Leaf(Sym),
}

/// All the `BitParserState` lists: `nodes[i]` is a 2-item list.
pub struct BitTrees {
    nodes: Vec<[Slot; 2]>,
}

impl BitTrees {
    fn new_node(&mut self) -> u32 {
        self.nodes.push([Slot::None, Slot::None]);
        (self.nodes.len() - 1) as u32
    }
}

/// `BitParser`: the bit position (`_pos`) and the shared `add` classmethod.
pub struct BitParser {
    pub _pos: i64,
}

impl BitParser {
    pub fn new() -> Self {
        BitParser { _pos: 0 }
    }

    /// `BitParser.add(root, v, bits)`.
    fn add(trees: &mut BitTrees, root: u32, v: Sym, bits: &str) {
        let mut p = root;
        let mut b: Option<usize> = None;
        for (i, c) in bits.bytes().enumerate() {
            if i > 0 {
                let bb = b.expect("b is not None");
                if let Slot::None = trees.nodes[p as usize][bb] {
                    let n = trees.new_node();
                    trees.nodes[p as usize][bb] = Slot::Node(n);
                }
                p = match trees.nodes[p as usize][bb] {
                    Slot::Node(n) => n,
                    // Python would raise TypeError at class creation; the tables are prefix-free.
                    _ => panic!("BitParser.add: code {bits} extends a leaf"),
                };
            }
            b = Some(if c == b'1' { 1 } else { 0 });
        }
        let bb = b.expect("b is not None");
        trees.nodes[p as usize][bb] = Slot::Leaf(v);
    }
}

impl Default for BitParser {
    fn default() -> Self {
        Self::new()
    }
}

/// The class attributes `MODE`, `WHITE`, `BLACK`, `UNCOMPRESSED` (root node indices).
pub const MODE: u32 = 0;
pub const WHITE: u32 = 1;
pub const BLACK: u32 = 2;
pub const UNCOMPRESSED: u32 = 3;

fn trees() -> &'static BitTrees {
    static TREES: OnceLock<BitTrees> = OnceLock::new();
    TREES.get_or_init(|| {
        let mut t = BitTrees { nodes: vec![] };
        for _ in 0..4 {
            t.new_node();
        }
        for (root, codes) in
            [(MODE, MODE_CODES), (WHITE, WHITE_CODES), (BLACK, BLACK_CODES), (UNCOMPRESSED, UNCOMPRESSED_CODES)]
        {
            for &(v, bits) in codes {
                BitParser::add(&mut t, root, v, bits);
            }
        }
        t
    })
}

// The `BitParser.add(...)` calls of the class body, in order.
const MODE_CODES: &[(Sym, &str)] = &[
    (Sym::Int(0), "1"),
    (Sym::Int(1), "011"),
    (Sym::Int(-1), "010"),
    (Sym::Str("h"), "001"),
    (Sym::Str("p"), "0001"),
    (Sym::Int(2), "000011"),
    (Sym::Int(-2), "000010"),
    (Sym::Int(3), "0000011"),
    (Sym::Int(-3), "0000010"),
    (Sym::Str("u"), "0000001111"),
    (Sym::Str("x1"), "0000001000"),
    (Sym::Str("x2"), "0000001001"),
    (Sym::Str("x3"), "0000001010"),
    (Sym::Str("x4"), "0000001011"),
    (Sym::Str("x5"), "0000001100"),
    (Sym::Str("x6"), "0000001101"),
    (Sym::Str("x7"), "0000001110"),
    (Sym::Str("e"), "000000000001000000000001"),
];
const WHITE_CODES: &[(Sym, &str)] = &[
    (Sym::Int(0), "00110101"),
    (Sym::Int(1), "000111"),
    (Sym::Int(2), "0111"),
    (Sym::Int(3), "1000"),
    (Sym::Int(4), "1011"),
    (Sym::Int(5), "1100"),
    (Sym::Int(6), "1110"),
    (Sym::Int(7), "1111"),
    (Sym::Int(8), "10011"),
    (Sym::Int(9), "10100"),
    (Sym::Int(10), "00111"),
    (Sym::Int(11), "01000"),
    (Sym::Int(12), "001000"),
    (Sym::Int(13), "000011"),
    (Sym::Int(14), "110100"),
    (Sym::Int(15), "110101"),
    (Sym::Int(16), "101010"),
    (Sym::Int(17), "101011"),
    (Sym::Int(18), "0100111"),
    (Sym::Int(19), "0001100"),
    (Sym::Int(20), "0001000"),
    (Sym::Int(21), "0010111"),
    (Sym::Int(22), "0000011"),
    (Sym::Int(23), "0000100"),
    (Sym::Int(24), "0101000"),
    (Sym::Int(25), "0101011"),
    (Sym::Int(26), "0010011"),
    (Sym::Int(27), "0100100"),
    (Sym::Int(28), "0011000"),
    (Sym::Int(29), "00000010"),
    (Sym::Int(30), "00000011"),
    (Sym::Int(31), "00011010"),
    (Sym::Int(32), "00011011"),
    (Sym::Int(33), "00010010"),
    (Sym::Int(34), "00010011"),
    (Sym::Int(35), "00010100"),
    (Sym::Int(36), "00010101"),
    (Sym::Int(37), "00010110"),
    (Sym::Int(38), "00010111"),
    (Sym::Int(39), "00101000"),
    (Sym::Int(40), "00101001"),
    (Sym::Int(41), "00101010"),
    (Sym::Int(42), "00101011"),
    (Sym::Int(43), "00101100"),
    (Sym::Int(44), "00101101"),
    (Sym::Int(45), "00000100"),
    (Sym::Int(46), "00000101"),
    (Sym::Int(47), "00001010"),
    (Sym::Int(48), "00001011"),
    (Sym::Int(49), "01010010"),
    (Sym::Int(50), "01010011"),
    (Sym::Int(51), "01010100"),
    (Sym::Int(52), "01010101"),
    (Sym::Int(53), "00100100"),
    (Sym::Int(54), "00100101"),
    (Sym::Int(55), "01011000"),
    (Sym::Int(56), "01011001"),
    (Sym::Int(57), "01011010"),
    (Sym::Int(58), "01011011"),
    (Sym::Int(59), "01001010"),
    (Sym::Int(60), "01001011"),
    (Sym::Int(61), "00110010"),
    (Sym::Int(62), "00110011"),
    (Sym::Int(63), "00110100"),
    (Sym::Int(64), "11011"),
    (Sym::Int(128), "10010"),
    (Sym::Int(192), "010111"),
    (Sym::Int(256), "0110111"),
    (Sym::Int(320), "00110110"),
    (Sym::Int(384), "00110111"),
    (Sym::Int(448), "01100100"),
    (Sym::Int(512), "01100101"),
    (Sym::Int(576), "01101000"),
    (Sym::Int(640), "01100111"),
    (Sym::Int(704), "011001100"),
    (Sym::Int(768), "011001101"),
    (Sym::Int(832), "011010010"),
    (Sym::Int(896), "011010011"),
    (Sym::Int(960), "011010100"),
    (Sym::Int(1024), "011010101"),
    (Sym::Int(1088), "011010110"),
    (Sym::Int(1152), "011010111"),
    (Sym::Int(1216), "011011000"),
    (Sym::Int(1280), "011011001"),
    (Sym::Int(1344), "011011010"),
    (Sym::Int(1408), "011011011"),
    (Sym::Int(1472), "010011000"),
    (Sym::Int(1536), "010011001"),
    (Sym::Int(1600), "010011010"),
    (Sym::Int(1664), "011000"),
    (Sym::Int(1728), "010011011"),
    (Sym::Int(1792), "00000001000"),
    (Sym::Int(1856), "00000001100"),
    (Sym::Int(1920), "00000001101"),
    (Sym::Int(1984), "000000010010"),
    (Sym::Int(2048), "000000010011"),
    (Sym::Int(2112), "000000010100"),
    (Sym::Int(2176), "000000010101"),
    (Sym::Int(2240), "000000010110"),
    (Sym::Int(2304), "000000010111"),
    (Sym::Int(2368), "000000011100"),
    (Sym::Int(2432), "000000011101"),
    (Sym::Int(2496), "000000011110"),
    (Sym::Int(2560), "000000011111"),
];
const BLACK_CODES: &[(Sym, &str)] = &[
    (Sym::Int(0), "0000110111"),
    (Sym::Int(1), "010"),
    (Sym::Int(2), "11"),
    (Sym::Int(3), "10"),
    (Sym::Int(4), "011"),
    (Sym::Int(5), "0011"),
    (Sym::Int(6), "0010"),
    (Sym::Int(7), "00011"),
    (Sym::Int(8), "000101"),
    (Sym::Int(9), "000100"),
    (Sym::Int(10), "0000100"),
    (Sym::Int(11), "0000101"),
    (Sym::Int(12), "0000111"),
    (Sym::Int(13), "00000100"),
    (Sym::Int(14), "00000111"),
    (Sym::Int(15), "000011000"),
    (Sym::Int(16), "0000010111"),
    (Sym::Int(17), "0000011000"),
    (Sym::Int(18), "0000001000"),
    (Sym::Int(19), "00001100111"),
    (Sym::Int(20), "00001101000"),
    (Sym::Int(21), "00001101100"),
    (Sym::Int(22), "00000110111"),
    (Sym::Int(23), "00000101000"),
    (Sym::Int(24), "00000010111"),
    (Sym::Int(25), "00000011000"),
    (Sym::Int(26), "000011001010"),
    (Sym::Int(27), "000011001011"),
    (Sym::Int(28), "000011001100"),
    (Sym::Int(29), "000011001101"),
    (Sym::Int(30), "000001101000"),
    (Sym::Int(31), "000001101001"),
    (Sym::Int(32), "000001101010"),
    (Sym::Int(33), "000001101011"),
    (Sym::Int(34), "000011010010"),
    (Sym::Int(35), "000011010011"),
    (Sym::Int(36), "000011010100"),
    (Sym::Int(37), "000011010101"),
    (Sym::Int(38), "000011010110"),
    (Sym::Int(39), "000011010111"),
    (Sym::Int(40), "000001101100"),
    (Sym::Int(41), "000001101101"),
    (Sym::Int(42), "000011011010"),
    (Sym::Int(43), "000011011011"),
    (Sym::Int(44), "000001010100"),
    (Sym::Int(45), "000001010101"),
    (Sym::Int(46), "000001010110"),
    (Sym::Int(47), "000001010111"),
    (Sym::Int(48), "000001100100"),
    (Sym::Int(49), "000001100101"),
    (Sym::Int(50), "000001010010"),
    (Sym::Int(51), "000001010011"),
    (Sym::Int(52), "000000100100"),
    (Sym::Int(53), "000000110111"),
    (Sym::Int(54), "000000111000"),
    (Sym::Int(55), "000000100111"),
    (Sym::Int(56), "000000101000"),
    (Sym::Int(57), "000001011000"),
    (Sym::Int(58), "000001011001"),
    (Sym::Int(59), "000000101011"),
    (Sym::Int(60), "000000101100"),
    (Sym::Int(61), "000001011010"),
    (Sym::Int(62), "000001100110"),
    (Sym::Int(63), "000001100111"),
    (Sym::Int(64), "0000001111"),
    (Sym::Int(128), "000011001000"),
    (Sym::Int(192), "000011001001"),
    (Sym::Int(256), "000001011011"),
    (Sym::Int(320), "000000110011"),
    (Sym::Int(384), "000000110100"),
    (Sym::Int(448), "000000110101"),
    (Sym::Int(512), "0000001101100"),
    (Sym::Int(576), "0000001101101"),
    (Sym::Int(640), "0000001001010"),
    (Sym::Int(704), "0000001001011"),
    (Sym::Int(768), "0000001001100"),
    (Sym::Int(832), "0000001001101"),
    (Sym::Int(896), "0000001110010"),
    (Sym::Int(960), "0000001110011"),
    (Sym::Int(1024), "0000001110100"),
    (Sym::Int(1088), "0000001110101"),
    (Sym::Int(1152), "0000001110110"),
    (Sym::Int(1216), "0000001110111"),
    (Sym::Int(1280), "0000001010010"),
    (Sym::Int(1344), "0000001010011"),
    (Sym::Int(1408), "0000001010100"),
    (Sym::Int(1472), "0000001010101"),
    (Sym::Int(1536), "0000001011010"),
    (Sym::Int(1600), "0000001011011"),
    (Sym::Int(1664), "0000001100100"),
    (Sym::Int(1728), "0000001100101"),
    (Sym::Int(1792), "00000001000"),
    (Sym::Int(1856), "00000001100"),
    (Sym::Int(1920), "00000001101"),
    (Sym::Int(1984), "000000010010"),
    (Sym::Int(2048), "000000010011"),
    (Sym::Int(2112), "000000010100"),
    (Sym::Int(2176), "000000010101"),
    (Sym::Int(2240), "000000010110"),
    (Sym::Int(2304), "000000010111"),
    (Sym::Int(2368), "000000011100"),
    (Sym::Int(2432), "000000011101"),
    (Sym::Int(2496), "000000011110"),
    (Sym::Int(2560), "000000011111"),
];
const UNCOMPRESSED_CODES: &[(Sym, &str)] = &[
    (Sym::Str("1"), "1"),
    (Sym::Str("01"), "01"),
    (Sym::Str("001"), "001"),
    (Sym::Str("0001"), "0001"),
    (Sym::Str("00001"), "00001"),
    (Sym::Str("00000"), "000001"),
    (Sym::Str("T00"), "00000011"),
    (Sym::Str("T10"), "00000010"),
    (Sym::Str("T000"), "000000011"),
    (Sym::Str("T100"), "000000010"),
    (Sym::Str("T0000"), "0000000011"),
    (Sym::Str("T1000"), "0000000010"),
    (Sym::Str("T00000"), "00000000011"),
    (Sym::Str("T10000"), "00000000010"),
];

/// `self._accept`: the bound method that receives the next decoded value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accept {
    ParseMode,
    ParseHoriz1,
    ParseHoriz2,
    ParseUncompressed,
}

fn invalid_data(msg: impl Into<String>) -> PyErr {
    PyErr::new(ExcKind::CCITTInvalidData, msg)
}

/// `array[i]` (read) with Python's index rules.
fn arr_get(a: &[i8], i: i64) -> PyResult<i8> {
    let n = a.len() as i64;
    let j = if i < 0 { i + n } else { i };
    if j < 0 || j >= n {
        return Err(PyErr::new(ExcKind::IndexError, "array index out of range"));
    }
    Ok(a[j as usize])
}

/// `array[i] = v` with Python's index rules.
fn arr_set(a: &mut [i8], i: i64, v: i64) -> PyResult<()> {
    let n = a.len() as i64;
    let j = if i < 0 { i + n } else { i };
    if j < 0 || j >= n {
        return Err(PyErr::new(ExcKind::IndexError, "array assignment index out of range"));
    }
    a[j as usize] = v as i8;
    Ok(())
}

/// `array.array("b", [1] * width)`.
fn new_line(width: i64) -> PyResult<Vec<i8>> {
    let n = width.max(0) as u64;
    // DIVERGE(ccitt-alloc): Python allocates a list of `width` pointers first and raises
    // MemoryError when that fails; the port only needs `width` bytes.
    if n > (isize::MAX as u64) / 8 {
        return Err(PyErr::new(ExcKind::MemoryError, ""));
    }
    let mut v: Vec<i8> = Vec::new();
    if v.try_reserve_exact(n as usize).is_err() {
        return Err(PyErr::new(ExcKind::MemoryError, ""));
    }
    v.resize(n as usize, 1);
    Ok(v)
}

/// `CCITTG4Parser`'s state.
pub struct CCITTG4Parser {
    pub bitparser: BitParser,
    pub _state: u32,
    pub _accept: Accept,
    pub width: i64,
    pub bytealign: bool,
    pub _color: i64,
    pub _y: i64,
    pub _curline: Vec<i8>,
    pub _refline: Vec<i8>,
    pub _curpos: i64,
    pub _n1: i64,
    pub _n2: i64,
}

impl CCITTG4Parser {
    /// `CCITTG4Parser(width, bytealign=False)`.
    pub fn new(width: i64, bytealign: bool) -> PyResult<Self> {
        let mut p = CCITTG4Parser {
            bitparser: BitParser::new(),
            _state: MODE,
            _accept: Accept::ParseMode,
            width,
            bytealign,
            _color: 1,
            _y: 0,
            _curline: vec![],
            _refline: vec![],
            _curpos: -1,
            _n1: 0,
            _n2: 0,
        };
        p.reset()?;
        Ok(p)
    }

    pub fn _get_bits(&self) -> String {
        // self._curline[: self._curpos]: a negative end counts from the end
        let len = self._curline.len() as i64;
        let end = if self._curpos < 0 { (len + self._curpos).max(0) } else { self._curpos.min(len) } as usize;
        self._curline[..end].iter().map(|b| b.to_string()).collect()
    }

    pub fn _get_refline(&self, i: i64) -> String {
        let join = |s: &[i8]| s.iter().map(|b| b.to_string()).collect::<String>();
        if i < 0 {
            "[]".to_string() + &join(&self._refline)
        } else if self._refline.len() as i64 <= i {
            join(&self._refline) + "[]"
        } else {
            let i = i as usize;
            join(&self._refline[..i]) + "[" + &self._refline[i].to_string() + "]" + &join(&self._refline[i + 1..])
        }
    }

    pub fn reset(&mut self) -> PyResult<()> {
        self._y = 0;
        self._curline = new_line(self.width)?;
        self._reset_line()?;
        self._accept = Accept::ParseMode;
        self._state = MODE;
        Ok(())
    }

    pub fn _reset_line(&mut self) -> PyResult<()> {
        let newline = new_line(self.width)?;
        self._refline = std::mem::replace(&mut self._curline, newline);
        self._curpos = -1;
        self._color = 1;
        Ok(())
    }

    pub fn _do_vertical(&mut self, dx: i64) -> PyResult<()> {
        let mut x1 = self._curpos + 1;
        loop {
            if x1 == 0 {
                if self._color == 1 && arr_get(&self._refline, x1)? as i64 != self._color {
                    break;
                }
            } else if x1 == self._refline.len() as i64
                || (arr_get(&self._refline, x1 - 1)? as i64 == self._color
                    && arr_get(&self._refline, x1)? as i64 != self._color)
            {
                break;
            }
            x1 += 1;
        }
        x1 += dx;
        let x0 = 0.max(self._curpos);
        let x1 = 0.max(self.width.min(x1));
        if x1 < x0 {
            for x in x1..x0 {
                arr_set(&mut self._curline, x, self._color)?;
            }
        } else if x0 < x1 {
            for x in x0..x1 {
                arr_set(&mut self._curline, x, self._color)?;
            }
        }
        self._curpos = x1;
        self._color = 1 - self._color;
        Ok(())
    }

    pub fn _do_pass(&mut self) -> PyResult<()> {
        let mut x1 = self._curpos + 1;
        loop {
            if x1 == 0 {
                if self._color == 1 && arr_get(&self._refline, x1)? as i64 != self._color {
                    break;
                }
            } else if x1 == self._refline.len() as i64
                || (arr_get(&self._refline, x1 - 1)? as i64 == self._color
                    && arr_get(&self._refline, x1)? as i64 != self._color)
            {
                break;
            }
            x1 += 1;
        }
        loop {
            if x1 == 0 {
                if self._color == 0 && arr_get(&self._refline, x1)? as i64 == self._color {
                    break;
                }
            } else if x1 == self._refline.len() as i64
                || (arr_get(&self._refline, x1 - 1)? as i64 != self._color
                    && arr_get(&self._refline, x1)? as i64 == self._color)
            {
                break;
            }
            x1 += 1;
        }
        for x in self._curpos..x1 {
            arr_set(&mut self._curline, x, self._color)?;
        }
        self._curpos = x1;
        Ok(())
    }

    pub fn _do_horizontal(&mut self, n1: i64, n2: i64) {
        if self._curpos < 0 {
            self._curpos = 0;
        }
        let mut x = self._curpos;
        let len = self._curline.len() as i64;
        // for _ in range(n1): if len(self._curline) <= x: break ...
        if x < len {
            let end = len.min(x + n1.max(0));
            for i in x..end {
                self._curline[i as usize] = self._color as i8;
            }
            x = x.max(end);
        }
        if x < len {
            let end = len.min(x + n2.max(0));
            for i in x..end {
                self._curline[i as usize] = (1 - self._color) as i8;
            }
            x = x.max(end);
        }
        self._curpos = x;
    }
}

/// The overridable part of `CCITTG4Parser` (`output_line`) and the methods that call it.
pub trait CCITTG4ParserImpl {
    fn parser(&mut self) -> &mut CCITTG4Parser;

    fn output_line(&mut self, y: i64, bits: &[i8]);

    fn feedbytes(&mut self, data: &[u8]) -> PyResult<()> {
        for &byte in data {
            let mut r = Ok(());
            for m in [128u8, 64, 32, 16, 8, 4, 2, 1] {
                r = self._parse_bit(byte & m != 0);
                if r.is_err() {
                    break;
                }
            }
            match r {
                Ok(()) => {}
                Err(e) if e.is(ExcKind::CCITTByteSkip) => {
                    let p = self.parser();
                    p._accept = Accept::ParseMode;
                    p._state = MODE;
                }
                Err(e) if e.is(ExcKind::CCITTEOFB) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn _parse_bit(&mut self, x: bool) -> PyResult<()> {
        let p = self.parser();
        let v = trees().nodes[p._state as usize][x as usize];
        p.bitparser._pos += 1;
        match v {
            Slot::Node(n) => p._state = n,
            Slot::None => self._call_accept(None)?,
            Slot::Leaf(s) => self._call_accept(Some(s))?,
        }
        Ok(())
    }

    /// `self._state = self._accept(v)`.
    fn _call_accept(&mut self, v: Option<Sym>) -> PyResult<()> {
        let state = match self.parser()._accept {
            Accept::ParseMode => self._parse_mode(v)?,
            Accept::ParseHoriz1 => self._parse_horiz1(v)?,
            Accept::ParseHoriz2 => self._parse_horiz2(v)?,
            Accept::ParseUncompressed => self._parse_uncompressed(v)?,
        };
        self.parser()._state = state;
        Ok(())
    }

    fn _parse_mode(&mut self, mode: Option<Sym>) -> PyResult<u32> {
        match mode {
            Some(Sym::Str("p")) => {
                self.parser()._do_pass()?;
                self._flush_line()?;
                Ok(MODE)
            }
            Some(Sym::Str("h")) => {
                let p = self.parser();
                p._n1 = 0;
                p._accept = Accept::ParseHoriz1;
                if p._color != 0 { Ok(WHITE) } else { Ok(BLACK) }
            }
            Some(Sym::Str("u")) => {
                self.parser()._accept = Accept::ParseUncompressed;
                Ok(UNCOMPRESSED)
            }
            Some(Sym::Str("e")) => Err(PyErr::new(ExcKind::CCITTEOFB, "")),
            Some(Sym::Int(dx)) => {
                self.parser()._do_vertical(dx)?;
                self._flush_line()?;
                Ok(MODE)
            }
            Some(other) => Err(invalid_data(other.py_str())),
            None => Err(invalid_data("None")),
        }
    }

    fn _parse_horiz1(&mut self, n: Option<Sym>) -> PyResult<u32> {
        let Some(n) = n else {
            return Err(invalid_data(""));
        };
        let Sym::Int(n) = n else { unreachable!("WHITE/BLACK hold ints") };
        let p = self.parser();
        p._n1 += n;
        if n < 64 {
            p._n2 = 0;
            p._color = 1 - p._color;
            p._accept = Accept::ParseHoriz2;
        }
        if p._color != 0 { Ok(WHITE) } else { Ok(BLACK) }
    }

    fn _parse_horiz2(&mut self, n: Option<Sym>) -> PyResult<u32> {
        let Some(n) = n else {
            return Err(invalid_data(""));
        };
        let Sym::Int(n) = n else { unreachable!("WHITE/BLACK hold ints") };
        let p = self.parser();
        p._n2 += n;
        if n < 64 {
            p._color = 1 - p._color;
            p._accept = Accept::ParseMode;
            let (n1, n2) = (p._n1, p._n2);
            p._do_horizontal(n1, n2);
            self._flush_line()?;
            Ok(MODE)
        } else if p._color != 0 {
            Ok(WHITE)
        } else {
            Ok(BLACK)
        }
    }

    fn _parse_uncompressed(&mut self, bits: Option<Sym>) -> PyResult<u32> {
        let bits = match bits {
            Some(Sym::Str(s)) if !s.is_empty() => s,
            _ => return Err(invalid_data("")),
        };
        if let Some(rest) = bits.strip_prefix('T') {
            let p = self.parser();
            p._accept = Accept::ParseMode;
            p._color = (rest.as_bytes()[0] - b'0') as i64;
            self._do_uncompressed(&rest[1..])?;
            Ok(MODE)
        } else {
            self._do_uncompressed(bits)?;
            Ok(UNCOMPRESSED)
        }
    }

    fn _flush_line(&mut self) -> PyResult<()> {
        let p = self.parser();
        if p.width <= p._curpos {
            let y = p._y;
            let line = std::mem::take(&mut p._curline);
            self.output_line(y, &line);
            let p = self.parser();
            p._curline = line;
            p._y += 1;
            p._reset_line()?;
            if p.bytealign {
                return Err(PyErr::new(ExcKind::CCITTByteSkip, ""));
            }
        }
        Ok(())
    }

    fn _do_uncompressed(&mut self, bits: &str) -> PyResult<()> {
        for c in bits.bytes() {
            let p = self.parser();
            let pos = p._curpos;
            arr_set(&mut p._curline, pos, (c - b'0') as i64)?;
            p._curpos += 1;
            self._flush_line()?;
        }
        Ok(())
    }
}

impl CCITTG4ParserImpl for CCITTG4Parser {
    fn parser(&mut self) -> &mut CCITTG4Parser {
        self
    }

    fn output_line(&mut self, y: i64, bits: &[i8]) {
        println!("{} {}", y, bits.iter().map(|b| b.to_string()).collect::<String>());
    }
}

/// `CCITTFaxDecoder(CCITTG4Parser)`.
pub struct CCITTFaxDecoder {
    pub base: CCITTG4Parser,
    pub reversed: bool,
    pub _buf: Vec<u8>,
}

impl CCITTFaxDecoder {
    pub fn new(width: i64, bytealign: bool, reversed: bool) -> PyResult<Self> {
        let base = CCITTG4Parser::new(width, bytealign)?;
        Ok(CCITTFaxDecoder { base, reversed, _buf: vec![] })
    }

    pub fn close(self) -> Vec<u8> {
        self._buf
    }
}

impl CCITTG4ParserImpl for CCITTFaxDecoder {
    fn parser(&mut self) -> &mut CCITTG4Parser {
        &mut self.base
    }

    fn output_line(&mut self, _y: i64, bits: &[i8]) {
        let mut arr = vec![0u8; bits.len().div_ceil(8)];
        for (i, &b) in bits.iter().enumerate() {
            let b = if self.reversed { 1 - b as i64 } else { b as i64 };
            if b != 0 {
                arr[i / 8] += [128u8, 64, 32, 16, 8, 4, 2, 1][i % 8];
            }
        }
        self._buf.extend_from_slice(&arr);
    }
}

/// `[1] * width`: the width must be an int (or bool), else `TypeError`.
fn repeat_count(v: &Value) -> PyResult<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        Value::Bool(b) => Ok(*b as i64),
        // `[1] * n` of a big n: `OverflowError`, or `[]` when negative (DIVERGE(bigint): the
        // width is then i64::MIN, not Python's value; only its sign is used).
        Value::BigInt(b) if num_traits::Signed::is_negative(&**b) => Ok(i64::MIN),
        Value::BigInt(_) => Err(PyErr::new(ExcKind::OverflowError, "cannot fit 'int' into an index-sized integer")),
        other => Err(PyErr::new(
            ExcKind::TypeError,
            format!("can't multiply sequence by non-int of type '{}'", other.type_name()),
        )),
    }
}

/// `ccittfaxdecode(data, params)`.
pub fn ccittfaxdecode(data: &[u8], params: &Value) -> PyResult<Vec<u8>> {
    let k = py_get_str(params, "K", Value::None)?;
    let mut parser = if py_eq(&k, &Value::Int(-1)) {
        let cols = py_get_str(params, "Columns", Value::None)?;
        let bytealign = py_get_str(params, "EncodedByteAlign", Value::None)?;
        let reversed = py_get_str(params, "BlackIs1", Value::None)?;
        CCITTFaxDecoder::new(repeat_count(&cols)?, bytealign.truthy(), reversed.truthy())?
    } else {
        return Err(PyErr::new(ExcKind::PDFValueError, k.py_str()));
    };
    parser.feedbytes(data)?;
    Ok(parser.close())
}

#[cfg(test)]
mod tests {
    use super::*;

    // tests/test_pdfminer_ccitt.py

    fn get_parser(bits: &str) -> CCITTG4Parser {
        let mut parser = CCITTG4Parser::new(bits.len() as i64, false).unwrap();
        parser._curline = bits.bytes().map(|c| (c - b'0') as i8).collect();
        parser._reset_line().unwrap();
        parser
    }

    #[test]
    fn test_b1() {
        let mut parser = get_parser("00000");
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._curpos, 0);
    }

    #[test]
    fn test_b2() {
        let mut parser = get_parser("10000");
        parser._do_vertical(-1).unwrap();
        assert_eq!(parser._curpos, 0);
    }

    #[test]
    fn test_b3() {
        let mut parser = get_parser("000111");
        parser._do_pass().unwrap();
        assert_eq!(parser._curpos, 3);
        assert_eq!(parser._get_bits(), "111");
    }

    #[test]
    fn test_b4() {
        let mut parser = get_parser("00000");
        parser._do_vertical(2).unwrap();
        assert_eq!(parser._curpos, 2);
        assert_eq!(parser._get_bits(), "11");
    }

    #[test]
    fn test_b5() {
        let mut parser = get_parser("11111111100");
        parser._do_horizontal(0, 3);
        assert_eq!(parser._curpos, 3);
        parser._do_vertical(1).unwrap();
        assert_eq!(parser._curpos, 10);
        assert_eq!(parser._get_bits(), "0001111111");
    }

    #[test]
    fn test_e1() {
        let mut parser = get_parser("10000");
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._curpos, 1);
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._curpos, 5);
        assert_eq!(parser._get_bits(), "10000");
    }

    #[test]
    fn test_e2() {
        let mut parser = get_parser("10011");
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._curpos, 1);
        parser._do_vertical(2).unwrap();
        assert_eq!(parser._curpos, 5);
        assert_eq!(parser._get_bits(), "10000");
    }

    #[test]
    fn test_e3() {
        let mut parser = get_parser("011111");
        parser._color = 0;
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._color, 1);
        assert_eq!(parser._curpos, 1);
        parser._do_vertical(-2).unwrap();
        assert_eq!(parser._color, 0);
        assert_eq!(parser._curpos, 4);
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._curpos, 6);
        assert_eq!(parser._get_bits(), "011100");
    }

    #[test]
    fn test_e4() {
        let mut parser = get_parser("10000");
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._curpos, 1);
        parser._do_vertical(-2).unwrap();
        assert_eq!(parser._curpos, 3);
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._curpos, 5);
        assert_eq!(parser._get_bits(), "10011");
    }

    #[test]
    fn test_e5() {
        let mut parser = get_parser("011000");
        parser._color = 0;
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._curpos, 1);
        parser._do_vertical(3).unwrap();
        assert_eq!(parser._curpos, 6);
        assert_eq!(parser._get_bits(), "011111");
    }

    #[test]
    fn test_e6() {
        let mut parser = get_parser("11001");
        parser._do_pass().unwrap();
        assert_eq!(parser._curpos, 4);
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._curpos, 5);
        assert_eq!(parser._get_bits(), "11111");
    }

    #[test]
    fn test_e7() {
        let mut parser = get_parser("0000000000");
        parser._curpos = 2;
        parser._color = 1;
        parser._do_horizontal(2, 6);
        assert_eq!(parser._curpos, 10);
        assert_eq!(parser._get_bits(), "1111000000");
    }

    #[test]
    fn test_e8() {
        let mut parser = get_parser("001100000");
        parser._curpos = 1;
        parser._color = 0;
        parser._do_vertical(0).unwrap();
        assert_eq!(parser._curpos, 2);
        parser._do_horizontal(7, 0);
        assert_eq!(parser._curpos, 9);
        assert_eq!(parser._get_bits(), "101111111");
    }

    #[test]
    fn test_m1() {
        let mut parser = get_parser("10101");
        parser._do_pass().unwrap();
        assert_eq!(parser._curpos, 2);
        parser._do_pass().unwrap();
        assert_eq!(parser._curpos, 4);
        assert_eq!(parser._get_bits(), "1111");
    }

    #[test]
    fn test_m2() {
        let mut parser = get_parser("101011");
        parser._do_vertical(-1).unwrap();
        parser._do_vertical(-1).unwrap();
        parser._do_vertical(1).unwrap();
        parser._do_horizontal(1, 1);
        assert_eq!(parser._get_bits(), "011101");
    }

    #[test]
    fn test_m3() {
        let mut parser = get_parser("10111011");
        parser._do_vertical(-1).unwrap();
        parser._do_pass().unwrap();
        parser._do_vertical(1).unwrap();
        parser._do_vertical(1).unwrap();
        assert_eq!(parser._get_bits(), "00000001");
    }

    // TestCCITTFaxDecoder

    #[test]
    fn test_ccittfaxdecoder_b1() {
        let mut decoder = CCITTFaxDecoder::new(5, false, false).unwrap();
        // decoder.output_line(0, b"0"): the bits are the bytes' values
        decoder.output_line(0, &[b'0' as i8]);
        assert_eq!(decoder.close(), b"\x80");
    }

    #[test]
    fn exception_classes() {
        assert!(ExcKind::CCITTInvalidData.is(ExcKind::PDFException));
        let mut params = crate::pyrt::value::Dict::new();
        params.insert("K".into(), Value::Int(-1));
        params.insert("Columns".into(), Value::Bool(true));
        let e = ccittfaxdecode(b"\x80\x80", &Value::dict(params)).unwrap_err();
        assert!(e.is(ExcKind::CCITTInvalidData));
        assert_eq!(e.msg, "None");
    }
}
