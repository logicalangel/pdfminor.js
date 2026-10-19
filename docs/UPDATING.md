# Updating to a new pdfminer.six release

The port follows one pdfminer.six release at a time, pinned as the `upstream/pdfminer.six`
submodule. `UPSTREAM.toml` records, for every upstream file, the version of it that its Rust
counterpart matches. An update is: move the submodule, let the tools list what changed, port each
diff, and prove with the difftest that the output is identical again.

pdfminer.six releases a few times a year and most releases touch a handful of files, so an update
is usually a small change set. Do one release at a time, even when several are pending: the diffs
stay small and each step can be checked.

## 1. Move the submodule

```sh
git -C upstream/pdfminer.six fetch --tags
git -C upstream/pdfminer.six tag --sort=-creatordate | head      # pick the next release
git -C upstream/pdfminer.six checkout <tag>
uv sync                                                         # the reference Python now runs <tag>
```

Read the release notes in `upstream/pdfminer.six/CHANGELOG.md` for the new version first; they
say which behaviour changed and why.

## 2. See what changed

```sh
uv run python tools/upstream.py status
```

prints one line per upstream file that changed since its Rust counterpart was last synced:

- `CHANGED pdfminer/layout.py -> port into crates/pdfminer/src/layout.rs (port)`
- `NEW pdfminer/foo.py` — a new module: add a `[[file]]` entry to `UPSTREAM.toml` and a new
  Rust module of the same name.
- `REMOVED ...` — remove or rework the Rust counterpart.

Data and generated files need no hand work:

```sh
uv run python tools/codegen.py      # glyphlist, latin_enc, fontmetrics, Python's unicode tables
```

The cmap files (`pdfminer/cmap/*.json.gz`) are embedded straight from the submodule at build
time, so new or changed cmaps need nothing.

## 3. Port the diffs

```sh
uv run python tools/upstream.py diff                      # every changed file
uv run python tools/upstream.py diff pdfminer/layout.py   # one file
```

For each hunk, find the same lines in the Rust file (the port keeps Python's order, names and
comments, so they are where you expect them) and make the same change, following
`docs/PORTING.md`. Changes to upstream tests (`tests/test_*.py`) go into the ported tests listed
in `UPSTREAM.toml`. Changes to `tools/pdf2txt.py` or `tools/dumppdf.py` go into the CLI.

When a file is ported, record it:

```sh
uv run python tools/upstream.py mark pdfminer/layout.py
```

## 4. Prove it

```sh
cargo test                                  # ported upstream tests + unit tests
uv run python tools/gen_testdata/<module>.py  # for each module whose generator exists: refresh
cargo test                                  # the differential test data, then test again
uv run python tools/difftest.py             # Python vs Rust on the whole corpus
```

The difftest must report `0 different`. A difference means one of:

1. A hunk was not ported (or ported differently). Fix the port.
2. New upstream behaviour the port's helpers do not cover yet (a new Python built-in in use, a
   new exception path). Extend `crates/pdfminer/src/pyrt` and document it in `docs/PORTING.md`.
3. A genuine, deliberate divergence. This is rare; it needs an entry in `docs/DIVERGENCES.md`
   and a `// DIVERGE(<id>)` comment.

Add the PDFs from the release's fixed issues (the new files under `upstream/pdfminer.six/samples`
are picked up automatically) and any private PDFs the release affects
(`PDFMINER_CORPUS=/path/to/pdfs uv run python tools/difftest.py`).

## 5. Finish

```sh
uv run python tools/upstream.py mark --all     # records the new release in UPSTREAM.toml
```

Then bump the version in `Cargo.toml` (`0.x.y+pdfminer.six.<tag>`) and
`crates/pdfminer/src/lib.rs` (`UPSTREAM_VERSION`) and in `crates/pdfminer-node/package.json`,
add a line to `CHANGELOG.md`, and commit everything in one commit:
`Update to pdfminer.six <tag>`.

## Using an AI agent for the port

The process above is mechanical enough to hand to a coding agent. Give it this file and
`docs/PORTING.md`, and the output of `tools/upstream.py status`. It should work one file at a
time, run `cargo test` after each file, and must not finish until `tools/difftest.py` reports no
differences.
