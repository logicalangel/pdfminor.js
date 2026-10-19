"""Differential test data for pdfminer/jbig2.py (JBIG2StreamReader + JBIG2StreamWriter).

    uv run python tools/gen_testdata/jbig2.py

Each case runs what ImageWriter._save_jbig2 does with the stream data: read the segments, then
write them as a JBIG2 file. The bytes written are recorded even when an exception is raised
(the file keeps what was written before it).
"""

import struct
import sys
from io import BytesIO
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import encode_input, encode_output, rng, sample_pdfs, write  # noqa: E402

from pdfminer.jbig2 import JBIG2StreamReader, JBIG2StreamWriter  # noqa: E402


def convert(data: bytes, fix_last_page: bool) -> dict:
    out = BytesIO()
    err = None
    try:
        segments = JBIG2StreamReader(BytesIO(data)).get_segments()
        JBIG2StreamWriter(out).write_file(segments, fix_last_page)
    except Exception as e:  # noqa: BLE001
        err = {"err": type(e).__name__, "msg": str(e)}
    res = encode_output(out.getvalue())
    if err:
        res.update(err)
    return res


def segment(r) -> bytes:
    number = r.choice([0, 1, 2, 255, 256, 257, 65536, 65537, 0xFFFFFFFE, 0xFFFFFFFF, r.randrange(1 << 32)])
    stype = r.choice([0, 6, 16, 22, 36, 38, 39, 48, 49, 50, 51, 52, 62, r.randrange(64)])
    deferred = r.random() < 0.1
    page_long = r.random() < 0.15
    flags = (0x80 if deferred else 0) | (0x40 if page_long else 0) | stype
    out = struct.pack(">LB", number, flags)
    # retention flags
    kind = r.random()
    if kind < 0.6:
        ref_count = r.choice([0, 0, 0, 1, 2, 4, 5, 6])
        out += bytes([(ref_count << 5) | r.randrange(32)])
    else:
        ref_count = r.choice([0, 1, 3, 4, 5, 7, 8, 9, 20])
        out += struct.pack(">L", (7 << 29) | ref_count)
        out += bytes(r.randrange(256) for _ in range((ref_count + 1 + 7) // 8))
    ref_size = 1 if number <= 256 else 4
    for _ in range(ref_count):
        out += r.randrange(1 << (8 * ref_size)).to_bytes(ref_size, "big")
    # page association
    page = r.choice([0, 1, 1, 2, 255, 256, 70000])
    out += struct.pack(">L", page) if page_long else bytes([page & 0xFF])
    # data
    if r.random() < 0.03:
        length = 0xFFFFFFFF
        payload = b""
    else:
        length = r.choice([0, 0, 1, 3, 19, 40])
        payload = bytes(r.randrange(256) for _ in range(length))
    out += struct.pack(">L", length) + payload
    return out


def inputs() -> list[tuple[bytes, bool]]:
    r = rng("jbig2")
    cases: list[tuple[bytes, bool]] = [(b"", True), (b"", False), (b"\x00", True)]
    for _ in range(600):
        n = r.choice([1, 1, 2, 3, 5])
        data = b"".join(segment(r) for _ in range(n))
        kind = r.random()
        if kind < 0.2:
            data = data[: r.randrange(len(data) + 1)]
        elif kind < 0.3 and data:
            b = bytearray(data)
            b[r.randrange(len(b))] = r.randrange(256)
            data = bytes(b)
        elif kind < 0.35:
            data = bytes(r.randrange(256) for _ in range(r.randrange(40)))
        cases.append((data, r.random() < 0.85))
    return cases


def sample_inputs() -> list[bytes]:
    """The stream data ImageWriter._save_jbig2 reads, for every JBIG2 image of the samples."""
    from pdfminer.pdfdocument import PDFDocument
    from pdfminer.pdfparser import PDFParser
    from pdfminer.pdftypes import LITERALS_JBIG2_DECODE, PDFStream

    out = []
    for path in sample_pdfs():
        try:
            with path.open("rb") as fp:
                doc = PDFDocument(PDFParser(fp))
                for xref in doc.xrefs:
                    for objid in xref.get_objids():
                        try:
                            obj = doc.getobj(objid)
                        except Exception:  # noqa: BLE001, S112
                            continue
                        if not isinstance(obj, PDFStream):
                            continue
                        filters = obj.get_filters()
                        if not any(f in LITERALS_JBIG2_DECODE for f, _ in filters):
                            continue
                        data = b""
                        for f, params in filters:
                            if f in LITERALS_JBIG2_DECODE and "JBIG2Globals" in params:
                                data += params["JBIG2Globals"].resolve().get_data().rstrip(b"\n")
                        out.append(data + obj.get_data())
        except Exception:  # noqa: BLE001, S112
            continue
    return out


def main() -> None:
    result = []
    for data, fix in inputs():
        result.append({**encode_input(data), "fix_last_page": fix, **convert(data, fix)})
    for data in sample_inputs():
        result.append({**encode_input(data), "fix_last_page": True, **convert(data, True)})
    write("jbig2", "convert", result)


if __name__ == "__main__":
    main()
