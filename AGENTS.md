# pdfminer-rs

A port of pdfminer.six to Rust whose output must be identical to the Python library's, with a
Node binding. Accuracy first, then speed.

- `upstream/pdfminer.six` — the pinned upstream (git submodule). Never edit it.
- `crates/pdfminer` — the port. One module per upstream module, same names and order.
  `src/pyrt/` holds the Python semantics the port needs (numbers, formatting, exceptions, `str`).
- `crates/pdfminer-cli` — `pdfminer-rs`: `pdf2txt`, `dumppdf` and `dump` (the layout JSON the
  difftests compare).
- `crates/pdfminer-node` — the Node binding (napi-rs), published as `@fuelfwd/pdfminer`.
- `tools/` — `codegen.py` (generated data tables), `oracle.py` (the Python reference dump),
  `difftest.py` (compares Python and Rust on a corpus), `upstream.py` (upstream sync status).

Read `docs/PORTING.md` before changing ported code and `docs/UPDATING.md` before moving to a new
upstream release. Every behaviour that differs from Python must be listed in
`docs/DIVERGENCES.md`.

Commands:

```sh
cargo test                                   # unit tests, including ported upstream tests
uv run python tools/difftest.py              # Python vs Rust on the corpus (must be empty)
uv run python tools/upstream.py status       # what changed upstream
```
