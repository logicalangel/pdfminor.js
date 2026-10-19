# Porting rules

The port's one goal is **the same output as pdfminer.six** for every input: the same text, the
same layout tree with bit-identical floats, the same exceptions on broken files. Speed comes
second and never at the cost of that. These rules are how we get there. Follow them for every new
module and for every upstream update.

## 1. Mirror the structure

- One Rust module per Python module, same name: `pdfminer/layout.py` → `crates/pdfminer/src/layout.rs`.
  `UPSTREAM.toml` lists every pair.
- Inside a module keep Python's order of classes, functions and statements, its names (snake_case
  stays, `LIT`, `KWD`, `do_Tf`, `LTChar` stay; use `#[allow(non_snake_case)]`), and its comments.
  Someone reading `git diff` of an upstream update must find the same lines in the Rust file.
- Start each file with `//! Port of \`pdfminer/<name>.py\`.`
- Data tables are generated, not ported: `tools/codegen.py` writes `glyphlist.rs`, `latin_enc.rs`,
  `fontmetrics.rs` and `pyrt/unicode_tables.rs` from the pinned Python. The cmaps are read from the
  submodule's `pdfminer/cmap/*.json.gz` at build time.
- Port the module's upstream tests (`tests/test_<name>.py`) into the module's `#[cfg(test)] mod
  tests` (or `crates/pdfminer/tests/` when they need sample files), with the upstream test names.

## 2. Classes

- State goes in a struct named like the class. Methods keep their Python names.
- A hierarchy whose methods are overridden (the parsers, `PDFDevice`) becomes a struct for the
  state plus a trait for the overridable methods; the trait's provided methods are the inherited
  algorithms (see `psparser.rs`: `PSStackParserImpl`).
- A closed hierarchy that callers inspect with `isinstance` (layout objects, fonts) becomes an
  enum or a struct with a `kind` enum. Keep an `is_<class>()` helper per `isinstance` check, and
  remember that `isinstance` includes subclasses (`LTTextLineHorizontal` is an `LTTextLine`).
- Objects Python shares by reference (`Rc`) stay shared; objects Python copies stay copied.
  Python's `is` is pointer identity (`Rc::ptr_eq`).

## 3. Python semantics: use `pyrt`, not the Rust equivalent

| Python | In the port |
|---|---|
| untyped object from the PDF (`Any`, `PDFStackT`, `object`) | `pyrt::Value`. Keep it a `Value` until Python itself converts it (`safe_float`, `int_value`, ...). |
| `isinstance(x, int)` | `x.is_int()` (true for `bool` too) |
| `x == y`, `x < y` on objects | `py_eq`, `py_cmp` (int/float compare exactly, `1 == 1.0`, `TypeError` for unorderable) |
| `int(x)`, `float(x)` | `py_int`, `py_float` (bytes, str, bool, NaN/inf rules) |
| `str(x)`, `repr(x)`, f-strings of numbers | `Value::py_str`/`repr`, `pyrt::fmt::{float_repr, fixed, general}`. **Never** Rust's `{}`/`{:?}` for a float that reaches output. |
| `a / b`, `a // b`, `a % b` | `pyrt::num::{div, floordiv, rem}` when `b` can be 0 (Python raises `ZeroDivisionError`, even for floats); `int_floordiv`/`int_mod` for ints (floor, not truncation). |
| `x ** y` | `pyrt::num::pow` |
| `min(a, b)`, `max(a, b)` | `pymin`, `pymax` (argument order decides NaN results; never `f64::min`) |
| `list.sort(key=...)`, `sorted` | `pyrt::sort` (stable; Timsort when keys are NaN) |
| `heapq` | `pyrt::heapq` (same algorithm, so ties pop in the same order) |
| `dict` | `IndexMap` (insertion order). |
| iterating a `set` | Python's order is hash order. Find out whether the order reaches the output; if it does, reproduce it and say so in a comment. |
| `str` holding extracted text | `PyStr` (keeps lone surrogates, which pdfminer produces) |
| `str` holding a name / key | `String` / `Rc<str>` |
| `str.isspace()`, `str.strip()` | `PyStr::isspace`/`strip`, `pyrt::ctype` (Python's whitespace is not Rust's) |
| `bytes.decode(..., "ignore")` etc. | `pyrt::codecs` |
| `zlib` | `pyrt::zlib` (real zlib, CPython's loops) |
| `io.BytesIO`, a file | `pyrt::io::BytesIO` (`BytesIO::file` for real files: negative seeks raise `OSError`) |
| `settings.STRICT` | `settings::STRICT` (const `false`); port the strict branches anyway. |
| `logging` | `log` macros; logging never changes output. |

### Numbers

Geometry (matrices, points, rects, font sizes) is `f64`. Python mixes ints and floats there
(`MATRIX_IDENTITY = (1, 0, 0, 1, 0, 0)`), but the values are identical, and every output
formats them with `%.3f`-style formats. Where Python *prints* a raw number with `str()`/`repr()`,
check whether it can be an int and reproduce the int formatting. Values Python keeps as ints
(`int_value`, cids, object ids) are `i64`.

### Exceptions

Every Python exception that can be raised is a `PyErr` with the same class (`pyrt::ExcKind`,
same hierarchy, e.g. `PDFKeyError` is a `KeyError`). That includes the implicit ones: `IndexError`
from `seq[i]`, `KeyError` from `d[k]`, `TypeError` from arithmetic on a wrong type, `ValueError`
from `int(x)` or tuple unpacking, `AssertionError` from `assert` (keep every `assert` whose
condition depends on input), `ZeroDivisionError`, `OverflowError`, `AttributeError`.

- A function that can raise returns `PyResult<T>`.
- `try/except A, B` becomes a `match` on `e.is_any(&[A, B])`; everything else propagates with `?`.
- Never `panic!`/`unwrap()` on anything that depends on the input. Panics are only for internal
  invariants.
- Python's recursion limit raises `RecursionError` (and `except Exception` catches it). Where
  Python recurses on input-controlled structure, keep a depth counter and raise `RecursionError`
  instead of overflowing the Rust stack.

## 4. Divergences

When matching Python exactly is impossible or absurd (ints beyond 64 bits, `charset_normalizer`
guessing an encoding), write `// DIVERGE(<id>): <what differs>` at the spot and add `<id>` to
`docs/DIVERGENCES.md` with the reason and the inputs it affects. Nothing else may differ.

## 5. Performance

Allowed: anything that cannot change a result. That means byte scans instead of regexes, fewer
copies, caches with the same invalidation, and parallel pages. Not allowed: a different algorithm,
reordered float operations (`a*b + c*d` must stay in that order; no `mul_add`), or a faster
approximation. Profile before optimizing.

## 6. Done means

A module is done when it compiles without warnings, its upstream tests are ported and pass, and
`tools/difftest.py` shows no differences on the corpus for everything that exercises it.
