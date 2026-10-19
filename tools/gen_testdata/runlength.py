"""Differential test data for pdfminer/runlength.py (rldecode).

    uv run python tools/gen_testdata/runlength.py
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import collect_sample_filter_calls, encode_input, record, rng, write  # noqa: E402

from pdfminer.runlength import rldecode  # noqa: E402


def encode(r, raw: bytes) -> bytes:
    """A valid RunLength encoding of raw, with randomly chosen run splits."""
    out = bytearray()
    i = 0
    while i < len(raw):
        j = i
        while j < len(raw) and raw[j] == raw[i] and j - i < 128:
            j += 1
        if j - i >= 2 and r.random() < 0.8:
            out += bytes([257 - (j - i), raw[i]])
            i = j
        else:
            n = min(len(raw) - i, r.randrange(1, 129))
            out += bytes([n - 1]) + raw[i : i + n]
            i += n
    return bytes(out)


def inputs() -> list[bytes]:
    r = rng("runlength")
    cases: list[bytes] = [
        b"",
        b"\x80",
        b"\x80junk",
        b"\x00",
        b"\x00a",
        b"\x01a",
        b"\x05123456\xfa7\x04abcde\x80junk",
        b"\x81",
        b"\x81a",
        b"\xff",
        b"\xffa",
        b"\x7f" + b"x" * 128,
        b"\x7f" + b"x" * 127,
        b"\xfa",
    ]
    for _ in range(800):
        n = r.choice([0, 1, 2, 5, 20, 100, 300])
        raw = bytes(r.choice([r.randrange(256), 7, 7, 7, 0]) for _ in range(n))
        enc = encode(r, raw)
        kind = r.random()
        if kind < 0.3:
            enc += b"\x80" + bytes(r.randrange(256) for _ in range(r.randrange(5)))
        elif kind < 0.5:
            enc = enc[: r.randrange(len(enc) + 1)]
        elif kind < 0.6 and enc:
            b = bytearray(enc)
            b[r.randrange(len(b))] = r.randrange(256)
            enc = bytes(b)
        elif kind < 0.7:
            enc = bytes(r.randrange(256) for _ in range(r.randrange(40)))
        cases.append(enc)
    return cases


def main() -> None:
    samples = collect_sample_filter_calls(["rldecode"])
    cases = [{**encode_input(c), **record(rldecode, c)} for c in inputs()]
    write("runlength", "rldecode", cases + samples["rldecode"])


if __name__ == "__main__":
    main()
