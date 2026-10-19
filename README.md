# pdfminer-rs

[pdfminer.six](https://github.com/pdfminer/pdfminer.six) ported to Rust, with a Node.js binding.

The port produces **the same output as pdfminer.six**: the same text, the same layout objects
with bit-identical coordinates, and the same errors on broken files. It follows a pinned
pdfminer.six release (now **20260107**) module by module, so new upstream releases can be
ported as small diffs.

## Use it from Node

```js
const { extractPages, extractText } = require("@fuelfwd/pdfminer");

const pages = await extractPages("file.pdf", { laparams: { charMargin: 2.0 } });
for (const page of pages) {
  for (const item of page.items) {
    if (item.type === "LTTextBoxHorizontal") console.log(item.index, item.bbox);
  }
}
const text = await extractText(fs.readFileSync("file.pdf"));
```

Pages are plain objects with the same class names and attributes as pdfminer's layout objects
(`LTPage`, `LTTextBoxHorizontal`, `LTTextLineHorizontal`, `LTChar`, `LTAnno`, `LTCurve`,
`LTRect`, `LTLine`, `LTImage`, `LTFigure`): `type`, `bbox`, `items`, and per type `text`,
`fontname`, `size`, `matrix`, `graphicstate`, ... See `crates/pdfminer-node/index.d.ts`.

## Use it from the command line

```sh
pdfminer-rs pdf2txt [pdf2txt.py options] file.pdf     # same options and output as pdf2txt.py
pdfminer-rs dumppdf [dumppdf.py options] file.pdf     # same as dumppdf.py
pdfminer-rs dump file.pdf                             # the full layout tree as JSON lines
```

## Layout of the repository

| Path | What |
|---|---|
| `upstream/pdfminer.six` | The pinned upstream release (git submodule). |
| `crates/pdfminer` | The port. `src/<module>.rs` is `pdfminer/<module>.py`. `src/pyrt/` reproduces the Python semantics the results depend on. |
| `crates/pdfminer-cli` | The `pdfminer-rs` command (`pdf2txt`, `dumppdf`, `dump`). |
| `crates/pdfminer-node` | The Node binding (napi-rs), package `@fuelfwd/pdfminer`. |
| `tools/` | `difftest.py` (Python vs Rust on a corpus), `oracle.py` (the Python reference dump), `upstream.py` (upstream sync status), `codegen.py` (generated tables), `gen_testdata/` (differential test data per module). |
| `UPSTREAM.toml` | For every upstream file: its Rust counterpart and the upstream version it matches. |
| `docs/` | `PORTING.md` (the rules), `UPDATING.md` (how to move to a new release), `DIVERGENCES.md` (the complete list of known differences). |

## Development

Requirements: Rust (pinned in `rust-toolchain.toml`), [uv](https://docs.astral.sh/uv/) (it
installs the reference Python and pdfminer.six from the submodule), Node 20+ for the binding.

```sh
git submodule update --init
uv sync
cargo test                                # unit tests and the ported upstream tests
uv run python tools/difftest.py           # must print "0 different"
```

Add your own PDFs to the comparison with `PDFMINER_CORPUS=/dir1:/dir2`.

To move to a new pdfminer.six release, follow `docs/UPDATING.md`.

## License

MIT, like pdfminer.six. See `LICENSE`.
