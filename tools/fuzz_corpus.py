"""Makes deterministic mutations of the corpus PDFs, to compare Python and Rust on broken input.

    uv run python tools/fuzz_corpus.py --count 2000 [--seed 1]   # writes tests/out/fuzz/*.pdf
    uv run python tools/difftest.py --corpus tests/out/fuzz -k layout-default

Real-world files rarely reach pdfminer's error paths; mutated files do, and every error path
must behave like Python's (same exception class, same partial output). Mutations: byte flips,
truncation, deleting or duplicating a span, replacing numbers with odd ones (huge, negative,
NaN-producing digit strings), swapping operator keywords, breaking stream lengths and xref
offsets, decompressing a stream in place and mutating its content stream. The same seed always
makes the same files.
"""

import argparse
import random
import re
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SAMPLES = ROOT / "upstream" / "pdfminer.six" / "samples"
OUT = ROOT / "tests" / "out" / "fuzz"

ODD_NUMBERS = [
    b"0", b"-0", b"-1", b"1e5", b"99999999999999999999", b"-99999999999999999999",
    b"1" + b"0" * 400 + b".0", b"-" + b"9" * 400 + b".5", b".", b"-.", b"+", b"0.000001",
    b"2147483648", b"-2147483649", b"65535", b"55296", b"1114112",
]
OPERATORS = [
    b"Tj", b"TJ", b"Tf", b"Td", b"TD", b"Tm", b"T*", b"'", b'"', b"BT", b"ET", b"q", b"Q",
    b"cm", b"re", b"m", b"l", b"c", b"h", b"S", b"f", b"n", b"W", b"Do", b"BI", b"ID", b"EI",
    b"rg", b"RG", b"g", b"k", b"cs", b"scn", b"Tc", b"Tw", b"Tz", b"TL", b"Ts", b"Tr", b"gs",
    b"BDC", b"EMC", b"d0", b"d1", b"sh",
]
NUMBER = re.compile(rb"(?<![A-Za-z0-9.#/])[-+]?\d+(\.\d+)?(?![A-Za-z0-9.])")
STREAM = re.compile(rb"stream\r?\n(.*?)endstream", re.S)


def inflate_streams(data: bytes) -> list[tuple[int, int, bytes]]:
    """(start, end, decoded) for each Flate stream that decodes cleanly."""
    out = []
    for m in STREAM.finditer(data):
        raw = m.group(1).rstrip(b"\r\n")
        try:
            out.append((m.start(1), m.start(1) + len(raw), zlib.decompress(raw)))
        except zlib.error:
            continue
    return out


def mutate_content(rng: random.Random, content: bytes) -> bytes:
    ops = rng.randint(1, 6)
    for _ in range(ops):
        k = rng.random()
        if k < 0.4:
            nums = list(NUMBER.finditer(content))
            if nums:
                m = rng.choice(nums)
                content = content[: m.start()] + rng.choice(ODD_NUMBERS) + content[m.end() :]
        elif k < 0.7:
            for op in OPERATORS:
                if rng.random() < 0.1:
                    content = content.replace(b" " + op + b"\n", b" " + rng.choice(OPERATORS) + b"\n", 1)
        elif k < 0.85:
            i = rng.randrange(len(content) + 1)
            content = content[:i] + rng.choice([b"[", b"]", b"<<", b">>", b"(", b")", b"<", b"/", b" null "]) + content[i:]
        else:
            i = rng.randrange(len(content) + 1)
            j = min(len(content), i + rng.randint(1, 50))
            content = content[:i] + content[j:]
    return content


def mutate(rng: random.Random, data: bytes) -> bytes:
    k = rng.random()
    if k < 0.15:
        # flip bytes
        b = bytearray(data)
        for _ in range(rng.randint(1, 20)):
            i = rng.randrange(len(b))
            b[i] = rng.randrange(256)
        return bytes(b)
    if k < 0.25:
        # truncate
        return data[: rng.randrange(len(data))]
    if k < 0.35:
        # delete or duplicate a span
        i = rng.randrange(len(data))
        j = min(len(data), i + rng.randint(1, 2000))
        if rng.random() < 0.5:
            return data[:i] + data[j:]
        return data[:j] + data[i:j] + data[j:]
    if k < 0.5:
        # odd numbers anywhere (object ids, lengths, offsets, operands)
        nums = list(NUMBER.finditer(data))
        for _ in range(rng.randint(1, 5)):
            if not nums:
                break
            m = rng.choice(nums)
            data = data[: m.start()] + rng.choice(ODD_NUMBERS) + data[m.end() :]
            nums = list(NUMBER.finditer(data))
        return data
    # decompress a content stream, mutate it and store it uncompressed (drop /Filter)
    streams = inflate_streams(data)
    if not streams:
        return mutate(rng, data)
    start, end, decoded = rng.choice(streams)
    new = mutate_content(rng, decoded)
    head = data[:start]
    d = head.rfind(b"<<")
    if d != -1:
        dict_part = head[d:]
        dict_part = re.sub(rb"/Filter\s*/FlateDecode", b"", dict_part)
        dict_part = re.sub(rb"/Filter\s*\[\s*/FlateDecode\s*\]", b"", dict_part)
        dict_part = re.sub(rb"/Length\s+\d+(\s+0\s+R)?", b"/Length %d" % len(new), dict_part)
        head = head[:d] + dict_part
    return head + new + data[end:]


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--count", type=int, default=500)
    p.add_argument("--seed", type=int, default=1)
    args = p.parse_args()
    sources = sorted(SAMPLES.rglob("*.pdf")) + sorted((ROOT / "tests" / "corpus").rglob("*.pdf"))
    sources = [s for s in sources if "encryption" not in s.parts]
    OUT.mkdir(parents=True, exist_ok=True)
    rng = random.Random(args.seed)
    for n in range(args.count):
        src = rng.choice(sources)
        data = src.read_bytes()
        for _ in range(rng.randint(1, 2)):
            data = mutate(rng, data)
        (OUT / f"fuzz-{args.seed}-{n:05d}-{src.stem}.pdf").write_bytes(data)
    print(f"wrote {args.count} files to {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
