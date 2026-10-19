"""Records pdfminer.six's layout analysis for the Rust differential test
(crates/pdfminer/tests/layout_analysis.rs), without the rest of the pipeline.

    uv run python tools/gen_testdata/layout.py

For every page of the sample PDFs (upstream samples and tests/corpus, a few pages per file) a
recording PDFLayoutAnalyzer captures what the analyzer received before the analysis: every
LTChar with the arguments it was built from and its final attributes, the curves, images and
figures (nested), and the page's bbox and rotation. Then the page is analysed under several
LAParams (each on a fresh copy of the page) and the result recorded: the tree of text boxes,
lines and groups with their bboxes (float repr), indices and the characters they hold. Synthetic
pages (NaN and infinite coordinates, duplicate positions, deep group chains) are added.

The Rust test rebuilds each page from the record, runs `analyze` and compares.

Determinism: `group_textboxes` breaks distance ties with `id(obj)`, a memory address in
CPython, so pdfminer.six's own result varies between runs on some files. The port uses the
creation order of the layout objects instead (docs/DIVERGENCES.md, `heap-id-ties`), and this
script makes Python do the same by replacing `id` in pdfminer.layout. The analysis runs at the
Python frame depth `extract_pages` gives it (7), so `RecursionError`s happen where they would.

Output: crates/pdfminer/tests/data/layout/<name>.json.gz, one per PDF plus synthetic.json.gz.
"""

import copy
import gzip
import inspect
import itertools
import json
import math
import sys
from pathlib import Path
from typing import Any

import pdfminer.layout as L
from pdfminer.converter import PDFLayoutAnalyzer
from pdfminer.pdfcolor import PREDEFINED_COLORSPACE
from pdfminer.pdfinterp import PDFGraphicState, PDFPageInterpreter, PDFResourceManager
from pdfminer.pdfpage import PDFPage
from pdfminer.psparser import PSLiteral

ROOT = Path(__file__).resolve().parents[2]
UPSTREAM = Path("/Users/paryarastegar/Dev/pdfminer-rs/upstream/pdfminer.six")
if not (UPSTREAM / "samples").is_dir():
    UPSTREAM = ROOT / "upstream" / "pdfminer.six"
OUT = ROOT / "crates" / "pdfminer" / "tests" / "data" / "layout"
MAX_PAGES = 2
MAX_CHARS = 4000  # pages with more characters are skipped (keeps the data small)
ANALYZE_FRAME_DEPTH = 7

# --- id() in creation order (DIVERGE(heap-id-ties)) ---------------------------------------
_serial = itertools.count()
_orig_component_init = L.LTComponent.__init__


def _component_init(self: Any, bbox: Any) -> None:
    self._serial = next(_serial)
    _orig_component_init(self, bbox)


L.LTComponent.__init__ = _component_init  # type: ignore[method-assign]
L.id = lambda o: o._serial  # type: ignore[attr-defined]

SETTINGS: dict[str, dict[str, Any]] = {
    "default": {},
    "flow_none": {"boxes_flow": None},
    "flow_m1": {"boxes_flow": -1},
    "flow_1": {"boxes_flow": 1},
    "vertical": {"detect_vertical": True},
    "all_texts": {"all_texts": True, "detect_vertical": True},
    "tight": {"char_margin": 0.5, "line_margin": 0.1, "word_margin": 0.5, "line_overlap": 0.2},
    "loose": {"char_margin": 20.0, "line_margin": 2.0, "word_margin": 0.0, "line_overlap": 0.9, "boxes_flow": 0.0},
}
NAN_SETTINGS: dict[str, dict[str, Any]] = {
    "nan_margins": {"char_margin": math.nan, "line_margin": math.nan, "word_margin": math.nan},
    "nan_overlap": {"line_overlap": math.nan, "detect_vertical": True},
}


def r(x: Any) -> str:
    return repr(float(x))


def rs(t: Any) -> list[str]:
    return [r(x) for x in t]


def text(s: str) -> Any:
    """A str as JSON; with lone surrogates (invalid JSON for serde) as a list of code points."""
    if any(0xD800 <= ord(c) <= 0xDFFF for c in s):
        return {"cps": [ord(c) for c in s]}
    return s


def enc(v: Any) -> Any:
    if v is None or isinstance(v, bool):
        return v
    if isinstance(v, int):
        return v
    if isinstance(v, float):
        return {"f": repr(v)}
    if isinstance(v, str):
        return {"s": text(v)}
    if isinstance(v, (bytes, bytearray)):
        return {"y": bytes(v).hex()}
    if isinstance(v, tuple):
        return {"t": [enc(x) for x in v]}
    if isinstance(v, list):
        return {"l": [enc(x) for x in v]}
    if isinstance(v, PSLiteral):
        return {"lit": v.name if isinstance(v.name, str) else repr(v.name)}
    return {"repr": repr(v)}


class Recorder(PDFLayoutAnalyzer):
    def __init__(self, rsrcmgr: PDFResourceManager) -> None:
        super().__init__(rsrcmgr, laparams=None)
        self.pages: list[L.LTPage] = []

    def begin_figure(self, name: str, bbox: Any, matrix: Any) -> None:
        super().begin_figure(name, bbox, matrix)
        self.cur_item._rec_bbox = bbox  # type: ignore[attr-defined]

    def render_char(self, matrix, font, fontsize, scaling, rise, cid, ncs, graphicstate):  # type: ignore[no-untyped-def]
        adv = super().render_char(matrix, font, fontsize, scaling, rise, cid, ncs, graphicstate)
        item = self.cur_item._objs[-1]
        vertical = font.is_vertical()
        item._rec = {
            "fontname": enc(font.fontname),
            "vertical": vertical,
            "descent": None if vertical else r(font.get_descent()),
            "fontsize": r(fontsize),
            "scaling": r(scaling),
            "rise": r(rise),
            "textwidth": r(font.char_width(cid)),
            "textdisp": enc(font.char_disp(cid)),
        }
        return adv

    def receive_layout(self, ltpage: L.LTPage) -> None:
        self.pages.append(ltpage)


def stable_name(name: str) -> str:
    """Inline images are named `str(id(obj))` by the interpreter: not reproducible."""
    return "inline" if name.isdigit() else name


def record_input(page: L.LTPage) -> tuple[dict, dict[int, int]]:
    """The page before the analysis; objects are numbered in pre-order."""
    index: dict[int, int] = {}

    def obj(o: Any) -> dict:
        i = len(index)
        index[id(o)] = i
        o._rec_index = i
        d: dict[str, Any] = {"k": type(o).__name__, "bbox": rs(o.bbox)}
        if isinstance(o, L.LTChar):
            d["text"] = text(o.get_text())
            d["matrix"] = rs(o.matrix)
            d["adv"] = r(o.adv)
            d["size"] = r(o.size)
            d["upright"] = o.upright
            d.update(getattr(o, "_rec", {}))
        elif isinstance(o, L.LTCurve):
            d["pts"] = [rs(p) for p in o.pts]
            d["linewidth"] = enc(o.linewidth)
        elif isinstance(o, L.LTImage):
            d["name"] = stable_name(o.name)
        elif isinstance(o, L.LTFigure):
            d["name"] = stable_name(o.name)
            d["matrix"] = rs(o.matrix)
            d["fbbox"] = rs(o._rec_bbox) if hasattr(o, "_rec_bbox") else None
            d["objs"] = [obj(x) for x in o]
        else:
            raise TypeError(type(o))
        return d

    rec = {
        "pageid": page.pageid,
        "bbox": rs(page.bbox),
        "rotate": enc(page.rotate),
        "objs": [obj(x) for x in page],
    }
    return rec, index


def clone_tree(o: Any) -> Any:
    """A copy of the page whose containers can be analysed again (leaves are shared)."""
    if isinstance(o, L.LTLayoutContainer):
        c = copy.copy(o)
        c._objs = [clone_tree(x) for x in o._objs]
        c.groups = None
        return c
    return o


def record_output(page: Any, index: dict[int, int]) -> dict:
    def node(o: Any) -> Any:
        if hasattr(o, "_rec_index"):
            if isinstance(o, L.LTFigure):
                return {"f": o._rec_index, "c": [node(x) for x in o], "g": groups(o)}
            return o._rec_index
        if isinstance(o, L.LTAnno):
            return {"a": text(o.get_text())}
        d: dict[str, Any] = {"t": type(o).__name__, "b": rs(o.bbox), "c": [node(x) for x in o]}
        if isinstance(o, L.LTTextBox):
            d["x"] = o.index
        return d

    def groups(o: Any) -> Any:
        """The group trees, flattened (they nest as deep as there are text boxes): groups in
        post-order, children referred to as {"box": index} or {"grp": position}."""
        if o.groups is None:
            return None
        nodes: list[dict] = []

        def ref(g: Any) -> dict:
            if not isinstance(g, L.LTTextGroup):
                return {"box": g.index}
            stack = [(g, False)]
            refs: list[dict] = []
            while stack:
                x, expanded = stack.pop()
                if not isinstance(x, L.LTTextGroup):
                    refs.append({"box": x.index})
                elif expanded:
                    n = len(x._objs)
                    children = refs[len(refs) - n:]
                    del refs[len(refs) - n:]
                    nodes.append({"t": type(x).__name__, "b": rs(x.bbox), "c": children})
                    refs.append({"grp": len(nodes) - 1})
                else:
                    stack.append((x, True))
                    for c in reversed(x._objs):
                        stack.append((c, False))
            return refs[0]

        roots = [ref(g) for g in o.groups]
        return {"nodes": nodes, "roots": roots}

    return {"c": [node(x) for x in page], "g": groups(page)}


def call_at_depth(depth: int, fn: Any, *args: Any) -> Any:
    """Calls fn(*args) so that fn's frame is at the given Python frame depth."""
    here = len(inspect.stack())  # this frame included

    def pad(k: int) -> Any:
        if k == 0:
            return fn(*args)
        return pad(k - 1)

    # pad(k) runs at depth here + 1 + (k0 - k); fn's frame is one deeper.
    k0 = depth - here - 2
    assert k0 >= 0
    return pad(k0)


def analyse(page: L.LTPage, settings: dict[str, dict[str, Any]]) -> tuple[dict, dict]:
    rec, index = record_input(page)
    results = {}
    for name, kw in settings.items():
        p = clone_tree(page)
        laparams = L.LAParams(**kw)
        try:
            call_at_depth(ANALYZE_FRAME_DEPTH, p.analyze, laparams)
            results[name] = record_output(p, index)
        except Exception as e:  # noqa: BLE001 - the class is the result
            results[name] = {"error": type(e).__name__, "msg": str(e)}
    return rec, results


def pdf_pages(path: Path) -> list[L.LTPage]:
    for password in ["", "foo", "user", "owner"]:
        try:
            with open(path, "rb") as fp:
                rsrcmgr = PDFResourceManager()
                device = Recorder(rsrcmgr)
                interpreter = PDFPageInterpreter(rsrcmgr, device)
                for i, page in enumerate(PDFPage.get_pages(fp, password=password)):
                    if i >= MAX_PAGES:
                        break
                    interpreter.process_page(page)
                return device.pages
        except Exception:  # noqa: BLE001
            if device.pages:
                return device.pages
            continue
    return []


def count_chars(o: Any) -> int:
    if isinstance(o, L.LTChar):
        return 1
    if isinstance(o, L.LTContainer):
        return sum(count_chars(x) for x in o)
    return 0


def settings_json() -> dict:
    out = {}
    for name, kw in {**SETTINGS, **NAN_SETTINGS}.items():
        lp = L.LAParams(**kw)
        out[name] = {
            "line_overlap": r(lp.line_overlap),
            "char_margin": r(lp.char_margin),
            "line_margin": r(lp.line_margin),
            "word_margin": r(lp.word_margin),
            "boxes_flow": None if lp.boxes_flow is None else r(lp.boxes_flow),
            "detect_vertical": lp.detect_vertical,
            "all_texts": lp.all_texts,
        }
    return out


# --- synthetic pages ----------------------------------------------------------------------


class FakeFont:
    def __init__(self, vertical: bool = False, descent: float = -0.2, fontname: Any = "Fake") -> None:
        self.vertical = vertical
        self.descent = descent
        self.fontname = fontname

    def is_vertical(self) -> bool:
        return self.vertical

    def get_descent(self) -> float:
        return self.descent


CS = PREDEFINED_COLORSPACE["DeviceGray"]


def char(page: Any, x: float, y: float, s: str = "x", size: float = 10.0, width: float = 0.5,
         font: FakeFont | None = None, textdisp: Any = 0, matrix: Any = None) -> None:
    font = font or FakeFont()
    m = matrix or (1, 0, 0, 1, x, y)
    c = L.LTChar(m, font, size, 1.0, 0, s, width, textdisp, CS, PDFGraphicState())
    c._rec = {
        "fontname": enc(font.fontname),
        "vertical": font.vertical,
        "descent": None if font.vertical else r(font.descent),
        "fontsize": r(size),
        "scaling": r(1.0),
        "rise": r(0),
        "textwidth": r(width),
        "textdisp": enc(textdisp),
    }
    page.add(c)


def word(page: Any, x: float, y: float, w: str, **kw: Any) -> None:
    for i, ch in enumerate(w):
        char(page, x + i * 5.0, y, ch, **kw)


def synthetic() -> list[tuple[str, L.LTPage, dict[str, dict[str, Any]]]]:
    nan, inf = math.nan, math.inf
    cases: list[tuple[str, L.LTPage, dict[str, dict[str, Any]]]] = []
    all_settings = {**SETTINGS, **NAN_SETTINGS}

    # Duplicate positions: identical words stacked and a regular grid (distance ties).
    p = L.LTPage(1, (0, 0, 612, 792))
    for _ in range(4):
        word(p, 100, 700, "same")
    for i in range(6):
        for j in range(4):
            word(p, 50 + j * 120, 600 - i * 30, "cell")
    for _ in range(3):
        word(p, 300, 100, "dup")
    cases.append(("grid-duplicates", p, all_settings))

    # NaN and infinite character coordinates.
    p = L.LTPage(1, (0, 0, 612, 792))
    word(p, 100, 700, "normal")
    word(p, nan, 650, "nanx")
    word(p, 100, nan, "nany")
    word(p, 100, 600, "after")
    char(p, 0, 0, "n", matrix=(nan, 0, 0, 1, 50, 50))
    word(p, inf, 500, "infx")
    word(p, 100, -inf, "infy")
    char(p, 0, 0, "i", matrix=(1, 0, 0, inf, 70, 70))
    word(p, 100, 400, "tail")
    word(p, 100, 388, "tail2")
    cases.append(("nan-inf-chars", p, all_settings))

    # NaN mixed into a column of boxes (NaN distances in the heap).
    p = L.LTPage(1, (0, 0, 612, 792))
    for i in range(8):
        word(p, 100, 700 - i * 40, "line%d" % i)
    word(p, 100, 300, "big", size=nan)
    word(p, 300, 300, "wide", width=nan)
    cases.append(("nan-sizes", p, all_settings))

    # NaN and infinite page bboxes: the Plane raises.
    for name, bbox in [("nan-page", (0, 0, nan, 792)), ("inf-page", (0, 0, inf, 792))]:
        p = L.LTPage(1, bbox)
        word(p, 100, 700, "hello")
        word(p, 100, 600, "world")
        cases.append((name, p, SETTINGS))

    # Whitespace-only lines (empties) and spaces between words.
    p = L.LTPage(1, (0, 0, 612, 792))
    word(p, 100, 700, "   ")
    word(p, 100, 680, "a b")
    word(p, 200, 680, "c")
    word(p, 100, 660, " ")
    cases.append(("empties", p, SETTINGS))

    # Vertical text, with textdisp tuples (None and numbers) and mixed orientation.
    p = L.LTPage(1, (0, 0, 612, 792))
    vf = FakeFont(vertical=True)
    for k in range(3):
        for i in range(5):
            char(p, 500 - k * 30, 700 - i * 12, "v", font=vf, textdisp=(None, 880),
                 matrix=(1, 0, 0, 1, 500 - k * 30, 700 - i * 12))
    for i in range(4):
        char(p, 300, 500 - i * 11, "w", font=vf, textdisp=(500, 1000), matrix=(1, 0, 0, 1, 300, 500 - i * 11))
    word(p, 100, 300, "horizontal")
    cases.append(("vertical", p, all_settings))

    # A figure with text inside (all_texts) and a nested figure.
    p = L.LTPage(1, (0, 0, 612, 792))
    word(p, 100, 700, "outside")
    f = L.LTFigure("F1", (0, 0, 300, 300), (1, 0, 0, 1, 50, 50))
    f._rec_bbox = (0, 0, 300, 300)
    word(f, 60, 100, "inside")
    word(f, 60, 88, "inside2")
    g = L.LTFigure("F2", (0, 0, 100, 100), (2, 0, 0, 2, 60, 200))
    g._rec_bbox = (0, 0, 100, 100)
    word(g, 70, 210, "nested")
    f.add(g)
    p.add(f)
    p.add(L.LTRect(1, (10, 10, 20, 20)))
    p.add(L.LTLine(0.5, (0, 0), (5, 5)))
    p.add(L.LTCurve(0, [(1, 1), (2, 5), (3, 2)]))
    cases.append(("figures", p, SETTINGS))

    # Deep group chains: Python raises RecursionError beyond 1000 frames.
    for name, n in [("deep-ok", 495), ("deep-recursion", 496)]:
        p = L.LTPage(1, (0, 0, 100, 100))
        y = 0.0
        for i in range(n):
            char(p, 10, y, "x", size=1.0, width=1.0, matrix=(1, 0, 0, 1, 10, y))
            y += 2.0 + i * 0.001
        cases.append((name, p, {"default": {}}))
    return cases


def write(name: str, data: dict) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    raw = json.dumps(data, separators=(",", ":"), ensure_ascii=True, allow_nan=False).encode()
    (OUT / f"{name}.json.gz").write_bytes(gzip.compress(raw, 9, mtime=0))


def main() -> None:
    sys.setrecursionlimit(1000)
    for old in OUT.glob("*.json.gz"):
        old.unlink()
    files = sorted((UPSTREAM / "samples").rglob("*.pdf")) + sorted((ROOT / "tests" / "corpus").glob("*.pdf"))
    total = 0
    for path in files:
        pages = pdf_pages(path)
        recs = []
        for page in pages:
            if count_chars(page) > MAX_CHARS:
                continue
            rec, results = analyse(page, SETTINGS)
            recs.append({"input": rec, "results": results})
        if not recs:
            continue
        rel = path.relative_to(UPSTREAM / "samples") if path.is_relative_to(UPSTREAM / "samples") else path.name
        name = str(rel).replace("/", "__").removesuffix(".pdf")
        write(name, {"file": str(rel), "settings": settings_json(), "pages": recs})
        total += len(recs)
        print(f"{name}: {len(recs)} pages")
    synth = []
    for name, page, settings in synthetic():
        rec, results = analyse(page, settings)
        synth.append({"name": name, "input": rec, "results": results})
    write("synthetic", {"file": "synthetic", "settings": settings_json(), "pages": synth})
    print(f"{total} PDF pages, {len(synth)} synthetic pages")


if __name__ == "__main__":
    main()
