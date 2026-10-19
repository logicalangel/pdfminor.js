"""Records pdfminer.six's converters (and the layout dump of tools/oracle.py) on hand-built pages,
for the Rust test crates/pdfminer/tests/converter_difftest.rs.

    uv run python tools/gen_testdata/converter.py

Each scenario is a list of operations on a PDFLayoutAnalyzer, the calls the interpreter makes:
begin_page (fake page with a mediabox), set_ctm, a character (LTChar built with a fake font that
answers fontname / is_vertical() / get_descent()), paint_path, begin_figure / end_figure,
render_image (a PDFStream with the given attributes and no data). Each scenario runs through every
converter configuration below: a fresh converter replays the operations, then end_page (which
analyses the page with the scenario's LAParams and calls receive_layout) and close(). The output
(bytes, or a str as UTF-8 with surrogates passed through) is recorded, with the exception and the
partial output when one is raised. The Rust test replays the same operations.

Determinism: tools/pyref.py (creation order for id()).

Output: crates/pdfminer/tests/data/converter/converter.json.gz
"""

import gzip
import io
import json
import math
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import pyref  # noqa: E402

pyref.apply()

import oracle  # noqa: E402
from pdfminer.converter import (  # noqa: E402
    HOCRConverter,
    HTMLConverter,
    PDFPageAggregator,
    TextConverter,
    XMLConverter,
)
from pdfminer.layout import LAParams, LTChar  # noqa: E402
from pdfminer.pdfcolor import PREDEFINED_COLORSPACE  # noqa: E402
from pdfminer.pdfinterp import PDFGraphicState, PDFResourceManager  # noqa: E402
from pdfminer.pdftypes import PDFObjRef, PDFStream  # noqa: E402
from pdfminer.psparser import KWD, PSLiteral  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "crates" / "pdfminer" / "tests" / "data" / "converter"


# --- values -------------------------------------------------------------------------------
# Compact JSON for Python objects: int/bool/null plain, {"f": repr}, {"s": str},
# {"y": hex}, {"t": [...]} tuple, {"l": [...]} list, {"lit": name}, {"d": [[k, v], ...]},
# {"ref": objid}, {"kwd": name}.


def F(x: float) -> dict:
    return {"f": repr(float(x))}


def val(v: Any) -> Any:
    if v is None or isinstance(v, (bool, int)):
        return v
    if isinstance(v, dict):
        k, x = next(iter(v.items()))
        if k == "f":
            return float(x)
        if k == "s":
            return x
        if k == "y":
            return bytes.fromhex(x)
        if k == "t":
            return tuple(val(i) for i in x)
        if k == "l":
            return [val(i) for i in x]
        if k == "lit":
            return PSLiteral(x)
        if k == "d":
            return {kk: val(vv) for kk, vv in x}
        if k == "ref":
            return PDFObjRef(None, x)
        if k == "kwd":
            return KWD(x.encode())
    raise TypeError(v)


def num(x: Any) -> float:
    return float(val(x))


def color(c: Any) -> Any:
    """null, {"int": i}, {"gray": f}, {"rgb": [...]}, {"cmyk": [...]}, {"pattern": name},
    {"base": color, "pattern": name}."""
    if c is None:
        return None
    if "int" in c:
        return c["int"]
    if "gray" in c:
        return float(c["gray"])
    if "rgb" in c or "cmyk" in c:
        return tuple(float(x) for x in c.get("rgb", c.get("cmyk")))
    if "base" in c:
        return (color(c["base"]), c["pattern"])
    return c["pattern"]


def text_of(t: Any) -> str:
    if isinstance(t, str):
        return t
    return "".join(chr(c) for c in t["cps"])


class FakeFont:
    def __init__(self, fontname: Any, vertical: bool, descent: float) -> None:
        self.fontname = fontname
        self.vertical = vertical
        self.descent = descent

    def is_vertical(self) -> bool:
        return self.vertical

    def get_descent(self) -> float:
        return self.descent


class FakePage:
    def __init__(self, mediabox: Any) -> None:
        self.mediabox = mediabox


def graphicstate(spec: dict) -> PDFGraphicState:
    gs = PDFGraphicState()
    for k, v in spec.items():
        if k in ("ncolor", "scolor"):
            setattr(gs, k, color(v))
        elif k in ("ncs", "scs"):
            setattr(gs, k, PREDEFINED_COLORSPACE[v])
        else:
            setattr(gs, k, val(v))
    return gs


def replay(dev: Any, ops: list) -> None:
    gstates: dict[int, PDFGraphicState] = {}
    for op in ops:
        kind = op["op"]
        if kind == "page":
            dev.set_ctm(tuple(num(x) for x in op["ctm"]))
            dev.begin_page(FakePage(tuple(num(x) for x in op["mediabox"])), dev.ctm)
        elif kind == "ctm":
            dev.set_ctm(tuple(num(x) for x in op["ctm"]))
        elif kind == "char":
            gid = op.get("gs_id", 0)
            if gid not in gstates:
                gstates[gid] = graphicstate(op.get("gs", {}))
            font = FakeFont(val(op["fontname"]), op["vertical"], num(op["descent"]))
            item = LTChar(
                tuple(num(x) for x in op["matrix"]),
                font,
                num(op["fontsize"]),
                num(op["scaling"]),
                num(op["rise"]),
                text_of(op["text"]),
                num(op["textwidth"]),
                val(op["textdisp"]),
                PREDEFINED_COLORSPACE[op["ncs"]],
                gstates[gid],
            )
            dev.cur_item.add(item)
        elif kind == "path":
            path = [tuple([seg[0], *(num(x) for x in seg[1:])]) for seg in op["path"]]
            dev.paint_path(graphicstate(op.get("gs", {})), op["stroke"], op["fill"], op["evenodd"], path)
        elif kind == "begin_figure":
            dev.begin_figure(op["name"], [val(x) for x in op["bbox"]], [val(x) for x in op["matrix"]])
        elif kind == "end_figure":
            dev.end_figure(op["name"])
        elif kind == "image":
            stream = PDFStream(val(op["attrs"]), b"")
            if "objid" in op:
                stream.set_objid(op["objid"], 0)
            dev.render_image(op["name"], stream)
        else:
            raise ValueError(kind)
    dev.end_page(None)


def laparams(spec: Any) -> Any:
    return None if spec is None else LAParams(**spec)


def configs() -> dict[str, Any]:
    """name -> (make(laparams) -> (device, getvalue))."""

    def text_str(lp: Any, showpageno: bool = False) -> Any:
        out = io.StringIO()
        dev = TextConverter(PDFResourceManager(), out, laparams=lp, showpageno=showpageno)
        return dev, lambda: out.getvalue().encode("utf-8", "surrogatepass")

    def text_bytes(lp: Any) -> Any:
        out = io.BytesIO()
        dev = TextConverter(PDFResourceManager(), out, laparams=lp, showpageno=True)
        return dev, out.getvalue

    def text_tio(errors: str) -> Any:
        def make(lp: Any) -> Any:
            buf = io.BytesIO()
            out = io.TextIOWrapper(buf, encoding="utf-8", errors=errors)
            dev = TextConverter(PDFResourceManager(), out, laparams=lp)

            def get() -> bytes:
                try:
                    out.flush()
                except Exception:  # noqa: BLE001, S110
                    pass
                return buf.getvalue()

            return dev, get

        return make

    def html(**kw: Any) -> Any:
        def make(lp: Any) -> Any:
            out = io.BytesIO()
            dev = HTMLConverter(PDFResourceManager(), out, laparams=lp, **kw)
            return dev, out.getvalue

        return make

    def html_str(lp: Any) -> Any:
        out = io.StringIO()
        dev = HTMLConverter(PDFResourceManager(), out, codec=None, laparams=lp, layoutmode="exact")
        return dev, lambda: out.getvalue().encode("utf-8", "surrogatepass")

    def xml(**kw: Any) -> Any:
        def make(lp: Any) -> Any:
            out = io.BytesIO()
            dev = XMLConverter(PDFResourceManager(), out, laparams=lp, **kw)
            return dev, out.getvalue

        return make

    def xml_str(lp: Any) -> Any:
        out = io.StringIO()
        dev = XMLConverter(PDFResourceManager(), out, codec=None, laparams=lp)
        return dev, lambda: out.getvalue().encode("utf-8", "surrogatepass")

    def hocr(**kw: Any) -> Any:
        def make(lp: Any) -> Any:
            out = io.BytesIO()
            dev = HOCRConverter(PDFResourceManager(), out, laparams=lp, **kw)
            return dev, out.getvalue

        return make

    def dump(lp: Any) -> Any:
        dev = PDFPageAggregator(PDFResourceManager(), laparams=lp)

        def get() -> bytes:
            return oracle.dumps(oracle.item(dev.get_result())).encode()

        return dev, get

    return {
        "text_str": text_str,
        "text_str_pageno": lambda lp: text_str(lp, True),
        "text_bytes": text_bytes,
        "text_tio_strict": text_tio("strict"),
        "text_tio_escape": text_tio("surrogateescape"),
        "html": html(),
        "html_exact_debug": html(scale=1.5, layoutmode="exact", debug=1),
        "html_loose": html(scale=2, fontscale=0.5, layoutmode="loose", showpageno=False, pagemargin=7),
        "html_latin1": html(codec="latin-1"),
        "html_str": html_str,
        "xml": xml(),
        "xml_strip": xml(stripcontrol=True),
        "xml_latin1": xml(codec="latin-1"),
        "xml_str": xml_str,
        "hocr": hocr(),
        "hocr_strip": hocr(stripcontrol=True, codec="utf-8"),
        "dump": dump,
    }


# --- scenarios ----------------------------------------------------------------------------

RGB = "DeviceRGB"
GRAY = "DeviceGray"


def char(
    text: Any,
    x: float,
    y: float,
    size: float = 10.0,
    width: float = 0.5,
    font: Any = None,
    vertical: bool = False,
    gs: Any = None,
    gs_id: int = 0,
    matrix: Any = None,
    ncs: str = GRAY,
    disp: Any = None,
) -> dict:
    m = matrix or [size, 0, 0, size, x, y]
    return {
        "op": "char",
        "text": text,
        "matrix": [F(v) for v in m],
        "fontname": font if font is not None else {"s": "ABCDEF+Helvetica"},
        "vertical": vertical,
        "descent": F(-0.2),
        "fontsize": F(1.0),
        "scaling": F(1.0),
        "rise": F(0.0),
        "textwidth": F(width),
        "textdisp": disp if disp is not None else ({"t": [None, F(880)]} if vertical else F(0)),
        "ncs": ncs,
        "gs": gs or {},
        "gs_id": gs_id,
    }


def word(s: str, x: float, y: float, **kw: Any) -> list:
    size = kw.get("size", 10.0)
    return [char(c, x + i * size * 0.5, y, **kw) for i, c in enumerate(s)]


def vword(s: str, x: float, y: float, size: float = 10.0, **kw: Any) -> list:
    return [char(c, x, y - i * size, size=size, vertical=True, **kw) for i, c in enumerate(s)]


PAGE = {"op": "page", "mediabox": [0, 0, 612, 792], "ctm": [1, 0, 0, 1, 0, 0]}
PAGE_ROT = {"op": "page", "mediabox": [10, 20, 400, 300], "ctm": [0, -1, 1, 0, -20, 400]}


def path(p: list, stroke: bool = True, fill: bool = False, evenodd: bool = False, gs: Any = None) -> dict:
    return {"op": "path", "path": [[s[0], *[F(v) for v in s[1:]]] for s in p], "stroke": stroke, "fill": fill,
            "evenodd": evenodd, "gs": gs or {}}


def scenarios() -> list[dict]:
    sc = []
    text_ops = (
        [PAGE]
        + word("Hello", 100, 700)
        + word("World", 140, 700)
        + word("Second line", 100, 686)
        + word("Far away", 400, 300, size=14.0, font={"s": "XYZ+Times-BoldItalic"},
               gs={"ncolor": {"rgb": [1, 0, 0]}, "ncs": RGB}, gs_id=1)
        + word("<&>\"'", 100, 100, font={"s": "A&B"})
    )
    for name, lp in [("default", {}), ("none", None), ("noflow", {"boxes_flow": None}),
                     ("tight", {"char_margin": 0.5, "word_margin": 0.05})]:
        sc.append({"name": f"text_{name}", "laparams": lp, "ops": text_ops})

    sc.append({
        "name": "vertical",
        "laparams": {"detect_vertical": True},
        "ops": [PAGE] + vword("縦書き", 300, 500) + vword("テキスト", 280, 500) + word("horizontal", 50, 50),
    })

    surrogates = [PAGE] + word("ab", 100, 700) + [
        char({"cps": [0xDC80]}, 110, 700),
        char({"cps": [0xD800, 0x41]}, 115, 700),
        char("\x01\x0b\x1f\x7f", 120, 700),
        char("(cid:12)", 125, 700),
        char("é中", 130, 700),
    ] + word("next", 100, 600)
    sc.append({"name": "surrogates", "laparams": {}, "ops": surrogates})
    sc.append({"name": "surrogates_raw", "laparams": None, "ops": surrogates})
    sc.append({"name": "escape_only_dc", "laparams": {},
               "ops": [PAGE] + word("ab", 100, 700) + [char({"cps": [0xDC80, 0xDCFF]}, 110, 700)] + word("cd", 100, 600)})

    shapes = [
        PAGE,
        path([("m", 10, 10), ("l", 100, 10)]),
        path([("m", 10, 20), ("l", 100, 20), ("h",)], gs={"linewidth": F(2.5), "dash": {"t": [{"l": [3, 1]}, 0]}}),
        path([("m", 10, 30), ("l", 100, 30), ("l", 100, 90), ("l", 10, 90), ("h",)], fill=True,
             gs={"ncolor": {"pattern": "P1"}, "scolor": {"base": {"rgb": [0.1, 0.2, 0.3]}, "pattern": "P2"}}),
        path([("m", 10, 30), ("l", 100, 30), ("l", 100, 90), ("l", 10, 90), ("l", 10, 30)]),
        path([("m", 10, 30), ("l", 100, 30), ("l", 100, 90), ("l", 10, 90), ("l", 10, 30), ("h",)]),
        path([("m", 110, 90), ("l", 190, 10), ("l", 190, 90), ("l", 110, 10), ("h",)], gs={"linewidth": 3}),
        path([("m", 72.41, 433.89), ("c", 72.41, 434.45, 71.96, 434.89, 71.41, 434.89)]),
        path([("m", 1, 1), ("v", 2, 2, 3, 3), ("y", 4, 4, 5, 5), ("h",), ("m", 6, 6), ("m", 7, 7), ("l", 8, 8)]),
        path([("h",)]),
        path([("l", 1, 2), ("l", 3, 4)]),
        path([("m", 5, 5), ("l", 5, 5), ("h",)]),
        path([("m", math.nan, 5), ("l", math.inf, -math.inf), ("l", 1, 1), ("l", math.nan, 5), ("h",)]),
        {"op": "ctm", "ctm": [2, 0, 0, 3, 10, 20]},
        path([("m", 1, 1), ("l", 2, 1), ("l", 2, 2), ("l", 1, 2), ("h",)], gs={"ncolor": {"cmyk": [0, 1, 0, 0.5]}}),
    ]
    sc.append({"name": "shapes", "laparams": {}, "ops": shapes})
    sc.append({"name": "shapes_raw", "laparams": None, "ops": shapes})
    sc.append({"name": "shapes_rotated", "laparams": {}, "ops": [PAGE_ROT] + shapes[1:6] + word("rot", 50, 50)})

    fig = [
        PAGE,
        *word("Before", 50, 700),
        {"op": "begin_figure", "name": "Fm1", "bbox": [0, 0, 200, 100], "matrix": [1, 0, 0, 1, 100, 400]},
        *word("Inside fig", 110, 450),
        path([("m", 0, 0), ("l", 10, 10)]),
        {"op": "begin_figure", "name": "Im1", "bbox": [0, 0, 1, 1], "matrix": [50, 0, 0, 40, 120, 410]},
        {"op": "image", "name": "Im1", "objid": 12,
         "attrs": {"d": [["Width", 10], ["Height", 5], ["BitsPerComponent", 8],
                         ["ColorSpace", {"lit": "DeviceRGB"}], ["Filter", {"lit": "DCTDecode"}]]}},
        {"op": "end_figure", "name": "Im1"},
        {"op": "end_figure", "name": "Fm1"},
        {"op": "begin_figure", "name": "inline1", "bbox": [0, 0, 1, 1], "matrix": [1, 0, 0, 1, 0, 0]},
        {"op": "image", "name": "inline1",
         "attrs": {"d": [["W", 2], ["H", 2], ["IM", True], ["CS", {"l": [{"lit": "Indexed"}, 1]}],
                         ["BPC", {"ref": 7}], ["X", {"kwd": "kw"}], ["Y", {"y": "00ff"}], ["Z", {"f": "nan"}]]}},
        {"op": "end_figure", "name": "inline1"},
        *word("After", 50, 100),
    ]
    sc.append({"name": "figures", "laparams": {}, "ops": fig})
    sc.append({"name": "figures_all_texts", "laparams": {"all_texts": True}, "ops": fig})
    sc.append({"name": "figures_raw", "laparams": None, "ops": fig})
    sc.append({"name": "figure_float_bbox", "laparams": {},
               "ops": [PAGE, {"op": "begin_figure", "name": "F", "bbox": [F(0.1), F(0.2), F(0.3), F(0.4)],
                              "matrix": [F(1.5), 0, 0, 2, F(0.1), 7]},
                       {"op": "end_figure", "name": "F"}]})
    sc.append({"name": "figure_bad_bbox", "laparams": {},
               "ops": [PAGE, {"op": "begin_figure", "name": "F", "bbox": [0, 0, 1], "matrix": [1, 0, 0, 1, 0, 0]}]})
    sc.append({"name": "figure_bad_matrix", "laparams": {},
               "ops": [PAGE, {"op": "begin_figure", "name": "F", "bbox": [0, 0, 1, 1], "matrix": [1, 0, 0, 1, 0]}]})
    sc.append({"name": "figure_none_bbox", "laparams": {},
               "ops": [PAGE, {"op": "begin_figure", "name": "F", "bbox": [0, None, 1, 1], "matrix": [1, 0, 0, 1, 0, 0]}]})
    sc.append({"name": "figure_str_bbox", "laparams": {},
               "ops": [PAGE, {"op": "begin_figure", "name": "F", "bbox": [{"s": "a"}, 0, {"s": "b"}, 1],
                              "matrix": [1, 0, 0, 1, 0, 0]}]})
    sc.append({"name": "unbalanced_figure", "laparams": {},
               "ops": [PAGE, {"op": "begin_figure", "name": "F", "bbox": [0, 0, 1, 1], "matrix": [1, 0, 0, 1, 0, 0]}]})
    sc.append({"name": "image_outside_figure", "laparams": {},
               "ops": [PAGE, {"op": "image", "name": "X", "attrs": {"d": []}}]})

    # (Bytes that are not UTF-8 are left out: HTML's make_compat_str guesses their encoding with
    # charset_normalizer, DIVERGE(charset-normalizer).)
    fonts = [PAGE]
    for i, f in enumerate([{"y": "414243"}, 7, None, {"l": [{"s": "Bold"}]}, {"s": "Plain"},
                           {"s": "Sub+Italic-Bold"}]):
        fonts += word(f"w{i}", 100, 700 - 20 * i, font=f)
    sc.append({"name": "fontnames", "laparams": {}, "ops": fonts})
    sc.append({"name": "fontname_bytes_only", "laparams": {}, "ops": [PAGE] + word("bb", 100, 700, font={"y": "4142"})})

    hocr_ops = [PAGE] + word("Mixed", 100, 700) + word("sizes", 130, 700, size=12.0) + word(
        "fonts", 165, 700, font={"s": "Arial-Bold"}) + word("a b", 100, 650) + word("  x  ", 100, 600)
    sc.append({"name": "hocr_words", "laparams": {}, "ops": hocr_ops})
    sc.append({"name": "hocr_words_raw", "laparams": None, "ops": hocr_ops})

    sc.append({"name": "empty", "laparams": {}, "ops": [PAGE]})
    sc.append({"name": "empty_raw", "laparams": None, "ops": [PAGE]})

    nan_ops = [PAGE, char("n", math.nan, 5), char("i", 5, math.inf), char("x", -1e300, 1e-300),
               char("y", 1e16, 0.1, matrix=[1, 0.5, -0.5, 1, 3, 4]), char("z", 0.0, -0.0)]
    sc.append({"name": "nan_inf", "laparams": {}, "ops": nan_ops})
    sc.append({"name": "nan_inf_raw", "laparams": None, "ops": nan_ops})

    many = [PAGE]
    for row in range(6):
        for col in range(4):
            many += word(f"r{row}c{col}", 50 + col * 130, 700 - row * 40)
    sc.append({"name": "many_boxes", "laparams": {}, "ops": many})
    sc.append({"name": "many_boxes_flow", "laparams": {"boxes_flow": -0.7}, "ops": many})

    gs_pattern = [PAGE] + word("pat", 100, 700, gs={"ncolor": {"pattern": "P9"}, "ncs": "Pattern"}, gs_id=2) + word(
        "unc", 100, 650, gs={"ncolor": {"base": {"gray": 0.5}, "pattern": "Q"}}, gs_id=3) + word(
        "lw", 100, 600, gs={"linewidth": F(1.0), "ncolor": {"gray": 0.25}}, gs_id=4)
    sc.append({"name": "colors", "laparams": {}, "ops": gs_pattern})
    return sc


def run(scenario: dict, make: Any) -> dict:
    get = None
    try:
        dev, get = make(laparams(scenario["laparams"]))
        replay(dev, scenario["ops"])
        if hasattr(dev, "close"):
            dev.close()
    except Exception as e:  # noqa: BLE001 - the class is the result
        res: dict = {"err": type(e).__name__, "msg": str(e)}
        if get is not None:
            try:
                res["out"] = get().hex()
            except Exception:  # noqa: BLE001, S110
                pass
        return res
    return {"out": get().hex()}


def main() -> None:
    cfgs = configs()
    cases = []
    for s in scenarios():
        results = {name: run(s, make) for name, make in cfgs.items()}
        cases.append({"scenario": s, "results": results})
    OUT.mkdir(parents=True, exist_ok=True)
    path = OUT / "converter.json.gz"
    with gzip.open(path, "wt", encoding="utf-8") as f:
        json.dump({"meta": {"generator": "tools/gen_testdata/converter.py", "python": sys.version.split()[0]},
                   "cases": cases}, f, separators=(",", ":"))
        f.write("\n")
    print(f"wrote {path.relative_to(ROOT)} ({len(cases)} scenarios x {len(cfgs)} converters)")


if __name__ == "__main__":
    main()
