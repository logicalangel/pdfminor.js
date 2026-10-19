"""The Python reference: what pdfminer.six outputs for a PDF, in the forms the port is compared on.

    uv run python tools/oracle.py layout FILE [--laparams JSON|none] [--password PW]
    uv run python tools/oracle.py pdf2txt FILE -- [pdf2txt.py options]

`layout` prints the canonical layout dump: every page that `extract_pages` yields, with every
attribute of every layout object, as JSON (one page per line, then a final status line). The Rust
CLI (`pdfminer-rs dump`) prints the same format, so the two can be compared byte for byte.

Format rules (keep them in sync with crates/pdfminer-cli/src/dump.rs):
- Geometry (bbox, matrix, pts, size, adv, linewidth, color components) is converted with
  float() first: Python mixes ints and floats there with identical values, and the port uses
  floats (docs/PORTING.md, "Numbers").
- Other values keep their Python types, encoded by `enc`: int -> int, float -> repr (json.dumps
  does that), str -> str, bytes -> {"bytes": hex}, PSLiteral -> {"lit": name},
  PSKeyword -> {"kwd": name}, list/tuple -> list, dict -> {"dict": [[k, v], ...]} (insertion
  order), PDFObjRef -> {"ref": objid}, PDFStream -> {"stream": objid}.
- Text keeps lone surrogates: JSON with ensure_ascii escapes them as \\udXXX.
- The last line is {"status": "ok"} or {"status": "error", "exc": ClassName, "msg": str(e)}
  (an exception after some pages were yielded keeps those pages, like Python).
"""

import argparse
import json
import math
import sys
from typing import Any

import pyref

pyref.apply()

from pdfminer.high_level import extract_pages  # noqa: E402
from pdfminer.layout import (
    LAParams,
    LTAnno,
    LTChar,
    LTComponent,
    LTContainer,
    LTCurve,
    LTFigure,
    LTImage,
    LTPage,
    LTTextBox,
    LTTextGroup,
)
from pdfminer.pdftypes import PDFObjRef, PDFStream
from pdfminer.psparser import PSKeyword, PSLiteral


def f(x: Any) -> float:
    return float(x)


def num(x: Any) -> Any:
    """A float written the way json.dumps writes it (NaN and infinities included)."""
    return float(x)


def enc(v: Any) -> Any:
    if v is None or isinstance(v, bool):
        return v
    if isinstance(v, int):
        return v
    if isinstance(v, float):
        return v
    if isinstance(v, str):
        return v
    if isinstance(v, (bytes, bytearray)):
        return {"bytes": bytes(v).hex()}
    if isinstance(v, PSLiteral):
        n = v.name
        return {"lit": n if isinstance(n, str) else {"bytes": n.hex()}}
    if isinstance(v, PSKeyword):
        return {"kwd": v.name.hex()}
    if isinstance(v, (list, tuple)):
        return [enc(x) for x in v]
    if isinstance(v, dict):
        return {"dict": [[enc(k), enc(x)] for k, x in v.items()]}
    if isinstance(v, PDFObjRef):
        return {"ref": v.objid}
    if isinstance(v, PDFStream):
        return {"stream": v.objid}
    return {"repr": repr(v)}


def color(c: Any) -> Any:
    """Color: gray number, (r, g, b), (c, m, y, k), pattern name, or (base, pattern name)."""
    if c is None:
        return None
    if isinstance(c, str):
        return {"pattern": c}
    if isinstance(c, tuple) and len(c) == 2 and isinstance(c[1], str):
        return {"base": color(c[0]), "pattern": c[1]}
    if isinstance(c, tuple):
        return [f(x) for x in c]
    return f(c)


def bbox(o: LTComponent) -> list[float]:
    return [f(o.x0), f(o.y0), f(o.x1), f(o.y1)]


def graphicstate(gs: Any) -> dict:
    return {
        "linewidth": f(gs.linewidth),
        "linecap": enc(gs.linecap),
        "linejoin": enc(gs.linejoin),
        "miterlimit": enc(gs.miterlimit),
        "dash": enc(gs.dash),
        "intent": enc(gs.intent),
        "flatness": enc(gs.flatness),
        "scolor": color(gs.scolor),
        "scs": gs.scs.name,
        "ncolor": color(gs.ncolor),
        "ncs": gs.ncs.name,
    }


def path(p: Any) -> Any:
    """original_path: (operator, (x, y), ...) segments, points already transformed."""
    if p is None:
        return None
    return [[seg[0], *[[f(x), f(y)] for x, y in seg[1:]]] for seg in p]


def item(o: Any) -> dict:
    d: dict[str, Any] = {"type": type(o).__name__}
    if isinstance(o, LTComponent):
        d["bbox"] = bbox(o)
    if isinstance(o, LTChar):
        d["text"] = o.get_text()
        d["fontname"] = enc(o.fontname)
        d["size"] = f(o.size)
        d["adv"] = f(o.adv)
        d["upright"] = o.upright
        d["matrix"] = [f(x) for x in o.matrix]
        d["ncs"] = o.ncs.name
        d["graphicstate"] = graphicstate(o.graphicstate)
    elif isinstance(o, LTAnno):
        d["text"] = o.get_text()
    elif isinstance(o, LTCurve):
        d["pts"] = [[f(x), f(y)] for x, y in o.pts]
        d["linewidth"] = f(o.linewidth)
        d["stroke"] = o.stroke
        d["fill"] = o.fill
        d["evenodd"] = o.evenodd
        d["stroking_color"] = color(o.stroking_color)
        d["non_stroking_color"] = color(o.non_stroking_color)
        d["original_path"] = path(o.original_path)
        d["dashing_style"] = enc(o.dashing_style)
    elif isinstance(o, LTImage):
        d["name"] = o.name
        d["srcsize"] = enc(o.srcsize)
        d["imagemask"] = enc(o.imagemask)
        d["bits"] = enc(o.bits)
        d["colorspace"] = enc(o.colorspace)
        d["stream"] = enc(o.stream)
    if isinstance(o, LTTextBox):
        d["index"] = o.index
    if isinstance(o, LTFigure):
        d["name"] = o.name
        d["matrix"] = [f(x) for x in o.matrix]
    if isinstance(o, LTPage):
        d["pageid"] = o.pageid
        d["rotate"] = f(o.rotate)
        d["groups"] = None if o.groups is None else [group(g) for g in o.groups]
    if isinstance(o, LTContainer):
        d["items"] = [item(x) for x in o]
    return d


def group(g: Any) -> dict:
    """Text groups (LTPage.groups) hold the text boxes already listed in the page: only refer
    to them by index."""
    if isinstance(g, LTTextGroup):
        return {"type": type(g).__name__, "bbox": bbox(g), "items": [group(x) for x in g]}
    return {"type": type(g).__name__, "index": g.index}


def dumps(o: Any) -> str:
    return json.dumps(o, ensure_ascii=True, separators=(",", ":"), allow_nan=True)


def laparams_from(spec: str | None) -> LAParams | None:
    if spec is None or spec == "default":
        return LAParams()
    if spec == "none":
        return None
    return LAParams(**json.loads(spec))


def cmd_layout(args: argparse.Namespace) -> int:
    laparams = laparams_from(args.laparams)
    out = sys.stdout
    try:
        if laparams is None:
            # extract_pages always analyses (it replaces None with LAParams()); without
            # analysis the aggregator yields the raw page.
            from pdfminer.converter import PDFPageAggregator
            from pdfminer.pdfinterp import PDFPageInterpreter, PDFResourceManager
            from pdfminer.pdfpage import PDFPage

            with open(args.file, "rb") as fp:
                rsrcmgr = PDFResourceManager()
                device = PDFPageAggregator(rsrcmgr, laparams=None)
                interpreter = PDFPageInterpreter(rsrcmgr, device)
                for page in PDFPage.get_pages(fp, password=args.password):
                    interpreter.process_page(page)
                    out.write(dumps(item(device.get_result())) + "\n")
        else:
            for page in extract_pages(args.file, password=args.password, laparams=laparams):
                out.write(dumps(item(page)) + "\n")
    except Exception as e:  # noqa: BLE001 - the class is the result
        out.write(dumps({"status": "error", "exc": type(e).__name__, "msg": str(e)}) + "\n")
        return 0
    out.write(dumps({"status": "ok"}) + "\n")
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    lay = sub.add_parser("layout")
    lay.add_argument("file")
    lay.add_argument("--laparams", default=None, help='"default", "none", or LAParams kwargs as JSON')
    lay.add_argument("--password", default="")
    lay.set_defaults(func=cmd_layout)
    args = p.parse_args()
    sys.setrecursionlimit(1000)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
