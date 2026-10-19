"""Differential test data for pdfminer/ascii85.py (ascii85decode, asciihexdecode).

    uv run python tools/gen_testdata/ascii85.py
"""

import base64
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import collect_sample_filter_calls, encode_input, record, rng, write  # noqa: E402

from pdfminer.ascii85 import ascii85decode, asciihexdecode  # noqa: E402

WS = [b" ", b"\t", b"\n", b"\r", b"\x0b", b"\x0c", b"\x00"]


def sprinkle(r, data: bytes, chars: list[bytes], rate: float) -> bytes:
    out = bytearray()
    for c in data:
        while r.random() < rate:
            out += r.choice(chars)
        out.append(c)
    while r.random() < rate:
        out += r.choice(chars)
    return bytes(out)


def a85_inputs() -> list[bytes]:
    r = rng("ascii85")
    cases: list[bytes] = [
        b"",
        b"~",
        b"~>",
        b"<~",
        b"<~~>",
        b"<~ ~>",
        b" < ~ abc ~ > ",
        b"z",
        b"zz",
        b"az",
        b"abcz",
        b"abcdez",
        b"y",
        b"!",
        b"!!",
        b"!!!",
        b"!!!!",
        b"!!!!!",
        b"s8W-!",
        b"s8W-\"",
        b"s8W-#",
        b"uuuuu",
        b"uuuu",
        b"u",
        b"v",
        b"~~",
        b"~>~>",
        b"abc~>~",
        b"abc~>>",
        b"abc~\n>\n",
        b"abc~>\n\n",
        b"abc\x0c~>",
        b"\x0cabc",
        b"ab\x0ccd",
        b"9jqo^BlbD-BleB1DJ+*+F(f,q",
        b"<~9jqo^BlbD-BleB1DJ+*+F(f,q~>",
        b"9jqo^\x80",
        b"9jqo^\xff",
        b"9jqo^\xe9",
        b"<<~abc~>",
        b"~<~abc",
        b"\n~\nabc",
        b"<\n~\nabc\n~\n>\n",
    ]
    alphabet = bytes(range(33, 118))
    for _ in range(1500):
        n = r.choice([0, 1, 2, 3, 4, 5, 7, 8, 12, 16, 31, 64, 100])
        raw = bytes(r.choice([0, 0x20, r.randrange(256)]) for _ in range(n))
        enc = base64.a85encode(raw, foldspaces=r.random() < 0.1, adobe=r.random() < 0.3)
        kind = r.random()
        if kind < 0.3:
            enc = sprinkle(r, enc, WS, 0.05)
        elif kind < 0.45:
            enc = enc[: r.randrange(len(enc) + 1)]
        elif kind < 0.6 and enc:
            b = bytearray(enc)
            for _ in range(r.randrange(1, 3)):
                b[r.randrange(len(b))] = r.randrange(256)
            enc = bytes(b)
        elif kind < 0.7:
            enc = r.choice([b"", b"~", b"<~", b" ~ ", b"<"]) + enc + r.choice([b"", b"~", b"~>", b" ~ > ", b">", b"~\n", b"\n"])
        elif kind < 0.8:
            enc = bytes(r.choice(alphabet + b"z~<> \n") for _ in range(r.randrange(30)))
        elif kind < 0.85:
            enc = bytes(r.randrange(256) for _ in range(r.randrange(20)))
        cases.append(enc)
    return cases


def ahx_inputs() -> list[bytes]:
    r = rng("asciihex")
    cases: list[bytes] = [
        b"",
        b">",
        b"0>",
        b"6",
        b"61",
        b"61>",
        b"6>",
        b">6",
        b"6 1",
        b"6\x0c1",
        b"6\x001",
        b"61 62 2e6364   65",
        b"61 62 2e6364   657>",
        b"7>",
        b"g0",
        b"0g",
        b"6g>",
        b"61>junk",
        b"6>>",
        b"AbCdEf",
        b"\xff\xff",
    ]
    hexchars = b"0123456789abcdefABCDEF"
    for _ in range(800):
        n = r.randrange(0, 40)
        s = bytes(r.choice(hexchars) for _ in range(n))
        kind = r.random()
        if kind < 0.3:
            s = sprinkle(r, s, WS, 0.1)
        if r.random() < 0.3:
            s = s[: r.randrange(len(s) + 1)] + b">" + s[r.randrange(len(s) + 1) :]
        if r.random() < 0.15 and s:
            b = bytearray(s)
            b[r.randrange(len(b))] = r.randrange(256)
            s = bytes(b)
        cases.append(s)
    return cases


def main() -> None:
    samples = collect_sample_filter_calls(["ascii85decode", "asciihexdecode"])
    a85 = [{**encode_input(c), **record(ascii85decode, c)} for c in a85_inputs()]
    ahx = [{**encode_input(c), **record(asciihexdecode, c)} for c in ahx_inputs()]
    write("ascii85", "ascii85decode", a85 + samples["ascii85decode"])
    write("ascii85", "asciihexdecode", ahx + samples["asciihexdecode"])


if __name__ == "__main__":
    main()
