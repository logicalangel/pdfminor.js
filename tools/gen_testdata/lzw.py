"""Differential test data for pdfminer/lzw.py (lzwdecode).

    uv run python tools/gen_testdata/lzw.py

Code streams are built by packing codes at the bit width pdfminer's own LZWDecoder expects at
that point (it is fed each code to follow its table), so they decode far, then are truncated,
corrupted or given out-of-table codes.
"""

import sys
from io import BytesIO
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import collect_sample_filter_calls, encode_input, record, rng, write  # noqa: E402

from pdfminer.lzw import CorruptDataError, LZWDecoder, lzwdecode  # noqa: E402


class BitWriter:
    def __init__(self) -> None:
        self.bits: list[int] = []

    def put(self, code: int, nbits: int) -> None:
        for i in range(nbits - 1, -1, -1):
            self.bits.append((code >> i) & 1)

    def tobytes(self, pad: int = 0) -> bytes:
        bits = self.bits + [pad] * (-len(self.bits) % 8)
        return bytes(int("".join(map(str, bits[i : i + 8])), 2) for i in range(0, len(bits), 8))


def code_stream(r, ncodes: int, clear_first: bool, p_bad: float, p_clear: float, p_eod: float) -> bytes:
    dec = LZWDecoder(BytesIO(b""))
    w = BitWriter()
    started = False
    for i in range(ncodes):
        nbits = dec.nbits
        if (i == 0 and clear_first) or r.random() < p_clear:
            code = 256
        elif r.random() < p_eod:
            code = 257
        elif r.random() < p_bad:
            code = r.randrange(1 << nbits)
        elif not started or not dec.prevbuf:
            code = r.randrange(256)
        else:
            # mostly valid codes: an existing entry or the next one
            code = r.choice([r.randrange(len(dec.table) + 1), len(dec.table), r.randrange(256)])
            code = min(code, (1 << nbits) - 1)
        w.put(code, nbits)
        try:
            dec.feed(code)
            started = started or code == 256
        except (CorruptDataError, IndexError):
            break
    return w.tobytes(pad=r.choice([0, 1]))


def inputs() -> list[bytes]:
    r = rng("lzw")
    cases: list[bytes] = [
        b"",
        b"\x00",
        b"\x80",
        b"\x80\x00",
        b"\x80\x0b\x60\x50\x22\x0c\x0c\x85\x01",
        b"\x80\x40",  # clear, then EOD
        b"\x00\x00\x00",  # code 0 without clear: IndexError
        b"\xff\xff\xff",
        b"\x80\x7f\xff",  # clear, then an out-of-table code
    ]
    for _ in range(600):
        kind = r.random()
        n = r.choice([1, 2, 3, 10, 50, 300, 600, 1200])
        if kind < 0.5:
            data = code_stream(r, n, True, 0.0, 0.002, 0.002)
        elif kind < 0.7:
            data = code_stream(r, n, r.random() < 0.8, 0.05, 0.01, 0.01)
        elif kind < 0.85:
            data = code_stream(r, n, True, 0.0, 0.0, 0.0)
            data = data[: r.randrange(len(data) + 1)]
        elif kind < 0.93:
            data = bytearray(code_stream(r, n, True, 0.0, 0.0, 0.0))
            for _ in range(r.randrange(1, 4)):
                if data:
                    data[r.randrange(len(data))] = r.randrange(256)
            data = bytes(data)
        else:
            data = bytes(r.randrange(256) for _ in range(r.randrange(30)))
        cases.append(data)
    # Long streams that fill the 12-bit table and keep going (codes stay below len(table)).
    for n in (5000, 9000):
        cases.append(code_stream(r, n, True, 0.0, 0.0, 0.0))
    return cases


def main() -> None:
    samples = collect_sample_filter_calls(["lzwdecode"])
    cases = [{**encode_input(c), **record(lzwdecode, c)} for c in inputs()]
    write("lzw", "lzwdecode", cases + samples["lzwdecode"])


if __name__ == "__main__":
    main()
