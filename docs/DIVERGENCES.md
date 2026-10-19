# Known divergences from pdfminer.six

Everything the port does differently from pdfminer.six, why, and which inputs it affects. Each
entry has an id that the code marks with `// DIVERGE(<id>)`. Nothing that is not listed here may
differ; a difference found by `tools/difftest.py` that is not in this list is a bug.

| id | What differs | Why | Affected inputs |
|---|---|---|---|
| `bigint` | Python ints are unbounded; the port uses `i64`. Integer tokens beyond i64 become the nearest float, int arithmetic that overflows continues in floats, `int(x)` of a float beyond i64 raises `OverflowError`, `nunpack` of more than 8 bytes keeps the low 63 bits. | Unbounded ints would slow every number for inputs that do not occur in real PDFs. | Malformed files with integers of 19+ digits. |
| `int-truediv` | `a / b` of two ints beyond 2**53 rounds twice. | Same as `bigint`. | Same as `bigint`. |
| `charset-normalizer` | `make_compat_str(bytes)` decodes UTF-8 or falls back to `str(bytes)`; Python guesses the encoding with charset_normalizer. | charset_normalizer is a heuristic with no stable specification. | `HTMLConverter`/`TagExtractor` output for font names or tag properties that are bytes and not UTF-8. |
