"""Reference data for the pdfdocument/pdfpage port: crates/pdfminer/tests/data/pdfdocument/*.json.

For every PDF under the upstream samples and the hand-made PDFs of `pdfsynth.py` (and broken variants of each: truncated at 1/2 and
3/4 of its length, with `startxref` defaced so that the fallback xref is used, and with
six bytes inserted after the header so that every offset is wrong, cut 40 bytes short, and
with random bytes overwritten), it dumps what
`PDFDocument` and `PDFPage` produce: the xrefs (object ids, positions, trailers), catalog, info,
`repr(doc.getobj(i))` of every object id (or the exception class), a hash of the decoded data of
streams whose filters are all Flate (or none), the pages, outlines and page labels.
`crates/pdfminer/tests/pdfdocument_diff.rs` computes the same with the port and compares.

    uv run python tools/gen_testdata/pdfdocument.py
"""

import hashlib
import itertools
import json
import logging
import pathlib
import sys

from pdfminer.pdfdocument import PDFDocument
from pdfminer.pdfpage import PDFPage
from pdfminer.pdfparser import PDFParser
from pdfminer.pdftypes import LITERALS_FLATE_DECODE, PDFStream
from pdfminer.psparser import PSLiteral

ROOT = pathlib.Path(__file__).resolve().parents[2]
SAMPLES = ROOT / "upstream/pdfminer.six/samples"
OUT = ROOT / "crates/pdfminer/tests/data/pdfdocument"
SYNTH = ROOT / "crates/pdfminer/tests/data/pdfdocument_synth"

# The passwords the upstream tests use.
PASSWORDS = {
    "encryption/aes-128.pdf": ["foo", ""],
    "encryption/aes-128-m.pdf": ["foo"],
    "encryption/aes-256.pdf": ["foo", ""],
    "encryption/aes-256-m.pdf": ["foo"],
    "encryption/aes-256-r6.pdf": ["usersecret", "ownersecret", "wrong"],
    "encryption/base.pdf": ["foo", ""],
    "encryption/rc4-40.pdf": ["foo"],
    "encryption/rc4-128.pdf": ["foo", "bar"],
    "encryption/encrypted_doc_no_id.pdf": [""],
}

MAX_REPR = 300
MAX_OUTLINES = 300


def short(s: str) -> str:
    if len(s) <= MAX_REPR:
        return s
    return "sha256:" + hashlib.sha256(s.encode("utf-8", "surrogatepass")).hexdigest()


def exc(e: BaseException) -> str:
    return "!" + type(e).__name__


def variants(data: bytes):
    yield "orig", data
    yield "half", data[: len(data) // 2]
    yield "3q", data[: len(data) * 3 // 4]
    yield "noxref", data.replace(b"startxref", b"startxreX")
    # Every offset off by 6 bytes.
    nl = data.find(b"\n") + 1
    yield "shifted", data[:nl] + b"%junk\n" + data[nl:]
    yield "tail", data[: max(0, len(data) - 40)]
    # Random bytes overwritten (an LCG the Rust test reproduces).
    state = len(data)
    for k in range(4):
        b = bytearray(data)
        for _ in range(6):
            state = (state * 6364136223846793005 + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
            pos = (state >> 33) % max(1, len(b))
            state = (state * 6364136223846793005 + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
            if b:
                b[pos] = (state >> 33) & 0xFF
        yield f"corrupt{k}", bytes(b)


def dump_doc(data: bytes, password: str) -> dict:
    out: dict = {}
    try:
        doc = PDFDocument(PDFParser(_File(data)), password=password)
    except Exception as e:  # noqa: BLE001
        out["open"] = exc(e)
        return out
    out["open"] = "ok"
    xrefs = []
    objids = []
    seen = set()
    for xref in doc.xrefs:
        x = {"repr": short(repr(xref)), "trailer": short(repr(xref.get_trailer())), "ids": []}
        try:
            for objid in xref.get_objids():
                try:
                    pos = repr(xref.get_pos(objid))
                except Exception as e:  # noqa: BLE001
                    pos = exc(e)
                x["ids"].append([repr(objid), pos])
                if repr(objid) not in seen:
                    seen.add(repr(objid))
                    objids.append(objid)
        except Exception as e:  # noqa: BLE001
            x["ids_error"] = exc(e)
        xrefs.append(x)
    out["xrefs"] = xrefs
    out["catalog"] = short(repr(doc.catalog))
    out["info"] = short(repr(doc.info))
    out["flags"] = [doc.is_printable, doc.is_modifiable, doc.is_extractable]
    objs = []
    for objid in objids:
        try:
            obj = doc.getobj(objid)
            r = short(repr(obj))
        except Exception as e:  # noqa: BLE001
            objs.append([repr(objid), exc(e), None])
            continue
        data_hash = None
        if isinstance(obj, PDFStream):
            try:
                filters = obj.get_filters()
                flate_only = all(isinstance(f, PSLiteral) and f in LITERALS_FLATE_DECODE for f, _ in filters)
            except Exception as e:  # noqa: BLE001
                filters, flate_only = None, False
                data_hash = "filters" + exc(e)
            if flate_only:
                try:
                    data_hash = hashlib.sha256(obj.get_data()).hexdigest()
                except Exception as e:  # noqa: BLE001
                    data_hash = exc(e)
        objs.append([repr(objid), r, data_hash])
    out["objects"] = objs

    pages = []
    try:
        for page in PDFPage.create_pages(doc):
            res = page.resources
            pages.append(
                {
                    "pageid": repr(page.pageid),
                    "label": page.label,
                    "mediabox": repr(page.mediabox),
                    "cropbox": repr(page.cropbox),
                    "rotate": page.rotate,
                    "resources": sorted(res.keys()) if isinstance(res, dict) else "type:" + type(res).__name__,
                    "contents": [short(repr(c)) for c in page.contents],
                    "attrs": list(page.attrs.keys()),
                    "repr": short(repr(page)),
                }
            )
    except Exception as e:  # noqa: BLE001
        pages.append(exc(e))
    out["pages"] = pages

    outlines = []
    try:
        for level, title, dest, action, se in itertools.islice(doc.get_outlines(), MAX_OUTLINES):
            outlines.append([level, title, short(repr(dest)), short(repr(action)), short(repr(se))])
    except Exception as e:  # noqa: BLE001
        outlines.append(exc(e))
    out["outlines"] = outlines

    labels = []
    try:
        for label in itertools.islice(doc.get_page_labels(), 12):
            labels.append(label)
    except Exception as e:  # noqa: BLE001
        labels.append(exc(e))
    out["labels"] = labels

    dests = []
    for name in (b"foo", "foo", b"Chapter1", "Chapter1", b"zzz"):
        try:
            dests.append(short(repr(doc.get_dest(name))))
        except Exception as e:  # noqa: BLE001
            dests.append(exc(e))
    out["dests"] = dests
    return out


class _File:
    """A binary file over bytes (negative seeks raise OSError like a real file)."""

    def __init__(self, data: bytes):
        import io

        self._b = io.BytesIO(data)

    def seek(self, pos, whence=0):
        if whence == 0 and pos < 0:
            raise OSError(22, "Invalid argument")
        return self._b.seek(pos, whence)

    def tell(self):
        return self._b.tell()

    def read(self, n=-1):
        return self._b.read(n)


def main():
    logging.disable(logging.CRITICAL)
    sys.setrecursionlimit(1000)
    OUT.mkdir(parents=True, exist_ok=True)
    for old in OUT.glob("*.json"):
        old.unlink()
    sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
    import pdfsynth

    pdfsynth.write_all(SYNTH)
    n = 0
    paths = [(p, p.relative_to(SAMPLES).as_posix()) for p in sorted(SAMPLES.rglob("*.pdf"))]
    paths += [(p, "synth/" + p.name) for p in sorted(SYNTH.glob("*.pdf"))]
    for path, rel in paths:
        data = path.read_bytes()
        runs = []
        for password in PASSWORDS.get(rel, [""]):
            for vname, vdata in variants(data):
                runs.append({"password": password, "variant": vname, **dump_doc(vdata, password)})
        name = rel.replace("/", "__").removesuffix(".pdf") + ".json"
        (OUT / name).write_text(json.dumps({"path": rel, "runs": runs}, indent=0, ensure_ascii=True))
        n += 1
    print(f"wrote {n} files to {OUT}")


if __name__ == "__main__":
    main()
