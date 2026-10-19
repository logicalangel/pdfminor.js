# Known divergences from pdfminer.six

Everything the port does differently from pdfminer.six, why, and which inputs it affects. Each
entry has an id that the code marks with `// DIVERGE(<id>)`. Nothing that is not listed here may
differ; a difference found by `tools/difftest.py` that is not in this list is a bug.

| id | What differs | Why | Affected inputs |
|---|---|---|---|
| `bigint` | Python ints are unbounded; the port uses `i64`. Integer tokens beyond i64 become the nearest float, int arithmetic that overflows continues in floats, `int(x)` of a float beyond i64 raises `OverflowError`, `nunpack` of more than 8 bytes keeps the low 63 bits. | Unbounded ints would slow every number for inputs that do not occur in real PDFs. | Malformed files with integers of 19+ digits. |
| `int-truediv` | `a / b` of two ints beyond 2**53 rounds twice. | Same as `bigint`. | Same as `bigint`. |
| `charset-normalizer` | `make_compat_str(bytes)` decodes UTF-8 or falls back to `str(bytes)`; Python guesses the encoding with charset_normalizer. | charset_normalizer is a heuristic with no stable specification. | `HTMLConverter`/`TagExtractor` output for font names or tag properties that are bytes and not UTF-8. |
| `recursion-depth` | Recursion that the input controls (`/Prev` chains, nested object streams, indirect `/Length` cycles, outline siblings, page and number trees) raises `RecursionError` after a fixed nesting budget (990 frames; Python frames between nested `getobj` calls are counted). | Python's limit applies to its whole stack, so the exact depth depends on the caller. | Malicious or cyclic files: the exception class is the same; only how far a finite but extremely deep chain gets differs. |
| `fp-repr` | Exception and log messages that Python formats with `repr(fp)` or `repr(parser)` (`PDFTextExtractionNotAllowed` from `get_pages`, `PDFNoValidXRef` messages of `PDFXRef.load`) say `<file>` / `<PDFParser>`. | The port has no Python object reprs for files. | Message text only; classes are the same. |
| `nonint-objid` | A stream fetched through a non-int object id keeps `objid = None` (Python stores the id object). | `PDFStream.objid` is an `i64`. | Unreachable in practice: non-int ids only come from broken object stream headers, and those entries never lead to a parsed stream. |
| `saslprep-unicode` | `saslprep` (AES-256 R6 passwords) uses the normalization data of the `unicode-normalization` crate, whose Unicode version can differ from Python's. | Python's `ucd_3_2_0.normalize` uses the current decomposition/composition data, as the port does, but of Python's Unicode version. | Passwords with characters assigned after the older of the two versions: always a `PDFValueError`, possibly with the other of its two messages. |
