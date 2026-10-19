"""Reference data for the `_saslprep` port: crates/pdfminer/tests/data/saslprep.json.

- `nfkc_single_sha256`: SHA-256 over `ucd_3_2_0.normalize("NFKC", chr(c))` for every scalar
  value c, each result UTF-8 encoded and followed by b"\\0".
- `cases`: random strings built from characters that exercise mapping, normalization
  (combining marks, Hangul, compatibility characters, the corrected CJK ideographs), bidi and the
  prohibition tables, with Python's `saslprep(s)` result or exception class and message.

    uv run python tools/gen_testdata/saslprep.py
"""

import hashlib
import json
import pathlib
import random
import unicodedata

from pdfminer._saslprep import saslprep

OUT = pathlib.Path(__file__).resolve().parents[2] / "crates/pdfminer/tests/data/saslprep.json"
ucd = unicodedata.ucd_3_2_0


def main():
    h = hashlib.sha256()
    for cp in range(0x110000):
        if 0xD800 <= cp < 0xE000:
            continue
        h.update(ucd.normalize("NFKC", chr(cp)).encode("utf-8") + b"\0")

    rng = random.Random(20260107)
    pool = (
        list("aAzZ09 ~")
        + [chr(c) for c in range(0x300, 0x370)]  # combining diacritics
        + [chr(c) for c in (0x00AD, 0x00A0, 0x1680, 0x2000, 0x200B, 0x3000, 0xFE00, 0x034F, 0x180B)]
        + [chr(c) for c in range(0x1100, 0x1113)]
        + [chr(c) for c in range(0x1161, 0x1176)]
        + [chr(c) for c in range(0x11A7, 0x11C3)]
        + [chr(c) for c in (0xAC00, 0xAC01, 0xD7A3, 0xD7A4)]
        + [chr(c) for c in (0x00AA, 0x2168, 0xFB01, 0x2126, 0x212B, 0x1E9B, 0x0344, 0x0F73, 0x0F75)]
        + [chr(c) for c in (0x2F868, 0x2F874, 0x2F91F, 0x2F95F, 0x2F9BF, 0xF951, 0xFA10)]
        + [chr(c) for c in (0x05D0, 0x05D1, 0x0627, 0x0628, 0x0661, 0x05B0, 0x064B)]
        + [chr(c) for c in (0x0007, 0x007F, 0x0085, 0x06DD, 0x200E, 0x202A, 0xE000, 0xFDD0, 0xFFFE)]
        + [chr(c) for c in (0x0221, 0x0870, 0x1B05, 0x1B35, 0x1F600, 0xE0001, 0xE0020, 0x2FF0)]
        + [chr(c) for c in (0x0B47, 0x0B3E, 0x0B57, 0x09C7, 0x09BE, 0x1025, 0x102E, 0x0CC6, 0x0CC2, 0x0CD5)]
        + [chr(c) for c in (0x0041, 0x030A, 0x0301, 0x0323, 0x0307, 0x0315, 0x031B, 0x05AE, 0x1DCE)]
    )
    cases = []
    seen = set()
    for _ in range(6000):
        n = rng.choice([1, 1, 2, 3, 3, 4, 5, 6, 8])
        s = "".join(rng.choice(pool) for _ in range(n))
        if s in seen:
            continue
        seen.add(s)
        try:
            r = {"ok": saslprep(s)}
        except Exception as e:  # noqa: BLE001
            r = {"exc": type(e).__name__, "msg": str(e)}
        cases.append({"in": s, "nfkc": ucd.normalize("NFKC", s), **r})
    OUT.write_text(json.dumps({"nfkc_single_sha256": h.hexdigest(), "cases": cases}, ensure_ascii=True, indent=0))
    print(f"wrote {OUT}: {len(cases)} cases")


if __name__ == "__main__":
    main()
