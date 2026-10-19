# Known divergences from pdfminer.six

Everything the port does differently from pdfminer.six, why, and which inputs it affects. Each
entry has an id that the code marks with `// DIVERGE(<id>)`. Nothing that is not listed here may
differ; a difference found by `tools/difftest.py` that is not in this list is a bug.

| id | What differs | Why | Affected inputs |
|---|---|---|---|
| `bigint` | Python ints are unbounded; the port uses `i64`. Integer tokens beyond i64 become the nearest float, int arithmetic that overflows continues in floats, `int(x)` of a float beyond i64 raises `OverflowError`, `nunpack` of more than 8 bytes keeps the low 63 bits. ToUnicode CMap codes and CFF INDEX offsets are exact up to 15 bytes (i128); a ToUnicode entry whose cid is beyond i64 is not stored (Python stores it under a big-int key that no cid can look up, visible only in `dump()`). | Unbounded ints would slow every number for inputs that do not occur in real PDFs. | Malformed files with integers of 19+ digits. |
| `int-truediv` | `a / b` of two ints beyond 2**53 rounds twice. | Same as `bigint`. | Same as `bigint`. |
| `charset-normalizer` | `make_compat_str(bytes)` decodes UTF-8 or falls back to `str(bytes)`; Python guesses the encoding with charset_normalizer. | charset_normalizer is a heuristic with no stable specification. | `HTMLConverter`/`TagExtractor` output for font names or tag properties that are bytes and not UTF-8. |
| `cmap-path` | `CMapDB` only knows the cmaps of the pinned `pdfminer/cmap` directory, embedded at build time. Python first looks in `$CMAP_PATH` (default `/usr/share/pdfminer/`), and an absolute cmap name can reach the installed package's directory through its real path. | The port has no installed package directory, and reading cmaps from the file system would make the output depend on the machine. | Machines with files in `$CMAP_PATH` or `/usr/share/pdfminer/`; PDFs naming a cmap by an absolute path. |
| `cmap-case` | Cmap file names are matched case-sensitively (as on Linux). On a case-insensitive file system (macOS by default) Python also loads `h.json.gz` for the cmap name `h`. | Python's own result depends on the file system; the port follows the case-sensitive one. | PDFs whose Encoding/CMap name differs from a pdfminer cmap only in case, run on macOS/Windows. |
