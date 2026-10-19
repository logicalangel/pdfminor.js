"""Differential test data for pdfminer/ccitt.py (ccittfaxdecode).

    uv run python tools/gen_testdata/ccitt.py

Valid Group 4 data comes from a small T.6 encoder below (code tables read from pdfminer's own
trees), optionally byte-aligned per row, with uncompressed-mode and EOFB codes mixed in; the
data is then truncated or corrupted, and decoded with assorted (also broken) DecodeParms.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import (  # noqa: E402
    collect_sample_filter_calls,
    encode_input,
    encode_value,
    record,
    rng,
    write,
)

from pdfminer.ccitt import CCITTG4Parser, ccittfaxdecode  # noqa: E402
from pdfminer.psparser import LIT  # noqa: E402


def codes_of(tree) -> dict:
    out = {}

    def walk(node, prefix):
        for b in (0, 1):
            v = node[b]
            if isinstance(v, list):
                walk(v, prefix + str(b))
            elif v is not None:
                out[v] = prefix + str(b)

    walk(tree, "")
    return out


MODE = codes_of(CCITTG4Parser.MODE)
WHITE = codes_of(CCITTG4Parser.WHITE)
BLACK = codes_of(CCITTG4Parser.BLACK)
UNCOMP = codes_of(CCITTG4Parser.UNCOMPRESSED)


def run_code(table: dict, n: int) -> str:
    s = ""
    while n >= 2560:
        s += table[2560]
        n -= 2560
    if n >= 64:
        s += table[(n // 64) * 64]
        n %= 64
    return s + table[n]


def encode_g4(rows: list[list[int]], width: int, r, bytealign: bool, extras: float) -> str:
    """T.6 encoding of rows (pixel 0 = white, 1 = black) as a bit string."""
    bits = ""
    ref = [0] * width

    def changes_after(line, a0, color):
        # first i > a0 (i >= 0) with line[i] != color... i.e. next changing element of the
        # opposite colour, else width
        i = max(a0 + 1, 0)
        while i < width and line[i] == color:
            i += 1
        return i

    def b1_of(a0, color):
        i = max(a0 + 1, 0)
        while i < width:
            prev = ref[i - 1] if i > 0 else 0
            if ref[i] != prev and ref[i] != color:
                return i
            i += 1
        return width

    for row in rows:
        a0 = -1
        color = 0
        while True:
            if r.random() < extras:
                # an uncompressed-mode excursion or a stray code
                kind = r.random()
                if kind < 0.5:
                    bits += MODE["u"] + "".join(r.choice(list(UNCOMP.values())) for _ in range(r.randrange(1, 6)))
                elif kind < 0.7:
                    bits += MODE["e"]
                else:
                    bits += r.choice(list(MODE.values()))
            a1 = changes_after(row, a0, color)
            b1 = b1_of(a0, color)
            b2 = b1
            while b2 < width and ref[b2] == ref[b1]:
                b2 += 1
            if b1 >= width:
                b2 = width
            if b2 < a1:
                bits += MODE["p"]
                a0 = b2
            elif abs(a1 - b1) <= 3:
                bits += MODE[a1 - b1]
                a0 = a1
                color = 1 - color
            else:
                a2 = changes_after(row, a1, 1 - color)
                t1, t2 = (WHITE, BLACK) if color == 0 else (BLACK, WHITE)
                bits += MODE["h"] + run_code(t1, a1 - max(a0, 0)) + run_code(t2, a2 - a1)
                a0 = a2
            if a0 >= width:
                break
        ref = row
        if bytealign:
            bits += "0" * (-len(bits) % 8)
    return bits


def tobytes(bits: str, pad: str) -> bytes:
    bits += pad * (-len(bits) % 8)
    return bytes(int(bits[i : i + 8], 2) for i in range(0, len(bits), 8))


def random_image(r, width: int, height: int) -> list[list[int]]:
    kind = r.random()
    rows = []
    prev = [0] * width
    for _ in range(height):
        if kind < 0.3:
            row = [1 if r.random() < 0.3 else 0 for _ in range(width)]
        elif kind < 0.7:
            # mostly like the previous row, with shifted edges
            row = prev[:]
            for _ in range(r.randrange(4)):
                a = r.randrange(width)
                b = min(width, a + r.randrange(1, max(2, width // 3)))
                c = r.choice([0, 1])
                for x in range(a, b):
                    row[x] = c
        else:
            row = [0] * width
            x = 0
            while x < width:
                n = r.choice([1, 2, 3, 5, 10, 63, 64, 65, 200, 3000])
                c = r.choice([0, 1])
                for i in range(x, min(width, x + n)):
                    row[i] = c
                x += n
        rows.append(row)
        prev = row
    return rows


def params(r, width: int, bytealign: bool) -> object:
    p = {"K": -1, "Columns": width}
    if bytealign or r.random() < 0.1:
        p["EncodedByteAlign"] = bytealign
    if r.random() < 0.2:
        p["BlackIs1"] = r.random() < 0.7
    if r.random() < 0.05:
        p["Rows"] = 3
    kind = r.random()
    if kind < 0.03:
        p["K"] = r.choice([0, 1, None, -1.0, True, LIT("x"), b"-1", -2])
        if p["K"] is None:
            del p["K"]
    elif kind < 0.08:
        p["Columns"] = r.choice([0, -3, 1, True, False, 1.0, None, b"8", LIT("C"), [8], width + 7, width - 1])
        if p["Columns"] is None:
            del p["Columns"]
    elif kind < 0.1:
        p["EncodedByteAlign"] = r.choice([1, 0, b"", [0], LIT("t")])
    elif kind < 0.11:
        return r.choice([None, [p], 5, b"x"])
    return p


def cases() -> list[tuple[bytes, object]]:
    r = rng("ccitt")
    out: list[tuple[bytes, object]] = []
    fixed = [
        (b"", {"K": -1, "Columns": 8}),
        (b"\x80", {"K": -1, "Columns": 8}),
        (b"\x80\x80", {"K": -1, "Columns": True}),
        (b"\xff\xff", {"K": -1, "Columns": 0}),
        (b"\x20\x00", {"K": -1, "Columns": 0}),
        (b"\x00\x10\x01", {"K": -1, "Columns": 8}),
        (b"\x02\x0f\xff", {"K": -1, "Columns": 8}),
        (b"\x00", {"K": 0}),
        (b"\x00", {}),
        (b"\x00", None),
        (b"\x00", [1]),
        (b"\x80", {"K": -1, "Columns": 1.5}),
    ]
    out += fixed
    for _ in range(700):
        width = r.choice([1, 2, 3, 5, 7, 8, 9, 15, 16, 17, 31, 64, 100, 200, 1728, 2600])
        height = r.choice([1, 2, 3, 5, 10]) if width < 1000 else r.choice([1, 2, 3])
        bytealign = r.random() < 0.2
        rows = random_image(r, width, height)
        extras = r.choice([0.0, 0.0, 0.0, 0.02, 0.1])
        bits = encode_g4(rows, width, r, bytealign, extras)
        if r.random() < 0.3:
            bits += MODE["e"] + MODE["e"]
        data = tobytes(bits, r.choice(["0", "1"]))
        kind = r.random()
        if kind < 0.15:
            data = data[: r.randrange(len(data) + 1)]
        elif kind < 0.3 and data:
            b = bytearray(data)
            for _ in range(r.randrange(1, 4)):
                b[r.randrange(len(b))] ^= 1 << r.randrange(8)
            data = bytes(b)
        elif kind < 0.35:
            data = bytes(r.randrange(256) for _ in range(r.randrange(40)))
        out.append((data, params(r, width, bytealign)))
    return out


def main() -> None:
    samples = collect_sample_filter_calls(["ccittfaxdecode"])
    result = []
    for data, p in cases():
        result.append({**encode_input(data), "params": encode_value(p), **record(ccittfaxdecode, data, p)})
    write("ccitt", "ccittfaxdecode", result + samples["ccittfaxdecode"])


if __name__ == "__main__":
    main()
