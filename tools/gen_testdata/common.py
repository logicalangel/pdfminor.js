"""Shared helpers for the differential test data generators.

Each generator in this directory runs the pinned pdfminer.six on many inputs and writes what it
returns (or the exception it raises) to `crates/pdfminer/tests/data/<module>/*.json`; the Rust
tests in `crates/pdfminer/tests/` check that the port gives the same. Regenerate after an
upstream update:

    uv run python tools/gen_testdata/<module>.py
"""

import base64
import hashlib
import json
import os
import random
import sys
import zlib
from pathlib import Path

from pdfminer.psparser import PSLiteral

ROOT = Path(__file__).resolve().parent.parent.parent
DATA = ROOT / "crates" / "pdfminer" / "tests" / "data"
# The submodule's samples (PDFMINER_SAMPLES overrides, e.g. in a worktree without the submodule).
SAMPLES = Path(os.environ.get("PDFMINER_SAMPLES", ROOT / "upstream" / "pdfminer.six" / "samples"))

# Outputs longer than this are stored as a SHA-256 digest.
MAX_INLINE = 4096


def rng(name: str) -> random.Random:
    """A deterministic RNG per generator, so regenerating gives the same cases."""
    return random.Random(name)


def record(fn, *args) -> dict:
    """Call fn(*args) and record its bytes result or its exception (class name and str)."""
    try:
        out = fn(*args)
    except Exception as e:  # noqa: BLE001 - every exception class is part of the behaviour
        return {"err": type(e).__name__, "msg": str(e)}
    return encode_output(out)


def encode_output(out: bytes) -> dict:
    if len(out) > MAX_INLINE:
        return {"out_len": len(out), "out_sha256": hashlib.sha256(out).hexdigest()}
    return {"out": out.hex()}


def encode_input(data: bytes) -> dict:
    """An input: hex, or zlib-compressed base64 when long."""
    if len(data) > MAX_INLINE:
        return {"in_z": base64.b64encode(zlib.compress(data, 9)).decode()}
    return {"in": data.hex()}


def encode_value(v: object) -> object:
    """A Python object from a PDF as JSON: plain JSON for None/bool/int/float(finite)/str-keyed
    dicts are tagged so the Rust side rebuilds the same `Value`."""
    if v is None or isinstance(v, bool) or isinstance(v, int):
        return v
    if isinstance(v, float):
        return {"t": "float", "v": repr(v)}
    if isinstance(v, bytes):
        return {"t": "bytes", "v": v.hex()}
    if isinstance(v, str):
        return {"t": "str", "v": v}
    if isinstance(v, PSLiteral):
        return {"t": "lit", "v": v.name if isinstance(v.name, str) else v.name.decode("latin-1")}
    if isinstance(v, list):
        return {"t": "list", "v": [encode_value(x) for x in v]}
    if isinstance(v, dict):
        return {"t": "dict", "v": [[k, encode_value(x)] for k, x in v.items()]}
    raise TypeError(f"cannot encode {type(v)}")


def write(module: str, name: str, payload: object) -> None:
    d = DATA / module
    d.mkdir(parents=True, exist_ok=True)
    path = d / f"{name}.json"
    meta = {
        "generator": f"tools/gen_testdata/{module}.py",
        "python": sys.version.split()[0],
    }
    from pdfminer import __version__

    meta["pdfminer"] = __version__
    with path.open("w") as f:
        json.dump({"meta": meta, "cases": payload}, f, separators=(",", ":"))
        f.write("\n")
    n = len(payload) if isinstance(payload, list) else "-"
    print(f"wrote {path.relative_to(ROOT)} ({n} cases, {path.stat().st_size} bytes)")


def sample_pdfs() -> list[Path]:
    return sorted(SAMPLES.rglob("*.pdf"))


def collect_sample_filter_calls(names: list[str]) -> dict[str, list[dict]]:
    """Decode every stream of every sample PDF with pdfminer, recording each call of the decoder
    functions `names` (as imported by pdfminer.pdftypes): its arguments and its result."""
    import pdfminer.pdftypes as pdftypes
    from pdfminer.pdfdocument import PDFDocument
    from pdfminer.pdfparser import PDFParser

    calls: dict[str, list[dict]] = {n: [] for n in names}
    originals = {n: getattr(pdftypes, n) for n in names}

    def wrap(n):
        orig = originals[n]

        def wrapper(*args):
            case = encode_input(args[0])
            if len(args) > 1:
                case["params"] = encode_value(args[1])
            case.update(record(orig, *args))
            calls[n].append(case)
            return orig(*args)

        return wrapper

    for n in names:
        setattr(pdftypes, n, wrap(n))
    try:
        for path in sample_pdfs():
            for password in ("", "foo", "baz", "owner", "user"):
                try:
                    with path.open("rb") as fp:
                        doc = PDFDocument(PDFParser(fp), password=password)
                        for xref in doc.xrefs:
                            for objid in xref.get_objids():
                                try:
                                    obj = doc.getobj(objid)
                                except Exception:  # noqa: BLE001, S112
                                    continue
                                if isinstance(obj, pdftypes.PDFStream):
                                    try:
                                        obj.get_data()
                                    except Exception:  # noqa: BLE001, S110
                                        pass
                    break
                except Exception:  # noqa: BLE001, S112
                    continue
    finally:
        for n in names:
            setattr(pdftypes, n, originals[n])
    # The same stream can be decoded twice (e.g. once per password try); keep unique cases.
    for n in names:
        seen = set()
        uniq = []
        for c in calls[n]:
            key = json.dumps(c, sort_keys=True)
            if key not in seen:
                seen.add(key)
                uniq.append(c)
        calls[n] = uniq
    return calls
