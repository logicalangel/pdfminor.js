"""Records what pdfminer.six's PDFPageInterpreter sends to its device, for the Rust differential
test (crates/pdfminer/tests/interp_difftest.rs).

    uv run python tools/gen_testdata/interp.py                          # samples + tests/corpus
    uv run python tools/gen_testdata/interp.py --corpus DIR --out DIR   # e.g. fuzzed files

For every PDF (the first MAX_PAGES pages) three runs, each on a freshly opened document with a
fresh PDFResourceManager and interpreter:

- "dev": a recording PDFDevice that logs every call with its arguments (the graphic state with
  int/float types, the text state, the TJ items, figure bboxes and matrices as given, tags with
  their properties, image names). Like every text device it iterates the TJ operand first.
- "text": a PDFTextDevice whose render_char logs its arguments and returns the advance a
  layout analyzer would (font.char_width(cid) * fontsize * scaling); the line matrix is logged
  after each string.
- "tag": the TagExtractor output (bytes).

A page that raises logs "error <class>" and the next page is processed. Logs longer than
MAX_LINES keep their first MAX_LINES lines plus the line count and the SHA-256 of the whole log.
Files Python needs more than TIMEOUT seconds for are skipped.

Inline images are named "inline<n>", counting from 1 per run (DIVERGE(inline-image-id)). Tag
properties whose bytes Python decodes with charset_normalizer (DIVERGE(charset-normalizer))
mark the run "out_diverges", and the test skips comparing its output bytes.
"""

import argparse
import gzip
import hashlib
import io
import itertools
import json
import logging
import signal
import sys
from pathlib import Path

import pdfminer.pdfinterp as pdfinterp
from pdfminer.pdfdevice import PDFDevice, PDFTextDevice, TagExtractor
from pdfminer.pdfdocument import PDFDocument
from pdfminer.pdfinterp import PDFPageInterpreter, PDFResourceManager
from pdfminer.pdfpage import PDFPage
from pdfminer.pdfparser import PDFParser
from pdfminer.utils import make_compat_str

ROOT = Path(__file__).resolve().parents[2]
UPSTREAM = ROOT / "upstream" / "pdfminer.six"
OUT = ROOT / "crates" / "pdfminer" / "tests" / "data" / "interp"
MAX_PAGES = 3
MAX_LINES = 3000
TIMEOUT = 30
PASSWORDS = ["", "foo", "baz", "owner", "user"]

logging.disable(logging.CRITICAL)


def f(x) -> str:
    return repr(float(x))


def mat(m) -> str:
    return "(" + ", ".join(f(x) for x in m) + ")"


def items(seq) -> str:
    return "[" + ", ".join(repr(x) for x in seq) + "]"


def cs(c) -> str:
    return f"{c.name}/{c.ncomponents!r}"


def gstate(gs) -> str:
    return (
        f"lw={gs.linewidth!r} cap={gs.linecap!r} join={gs.linejoin!r} miter={gs.miterlimit!r} "
        f"dash={gs.dash!r} intent={gs.intent!r} flat={gs.flatness!r} "
        f"sc={gs.scolor!r} scs={cs(gs.scs)} nc={gs.ncolor!r} ncs={cs(gs.ncs)}"
    )


def path_str(path) -> str:
    return ";".join(",".join([seg[0]] + [repr(float(a)) for a in seg[1:]]) for seg in path)


def short_sha(b) -> str:
    return hashlib.sha256(b or b"").hexdigest()[:16]


class Recorder(PDFDevice):
    def __init__(self, rsrcmgr, log):
        super().__init__(rsrcmgr)
        self.log = log

    def set_ctm(self, ctm):
        self.ctm = ctm
        self.log.append("ctm " + mat(ctm))

    def begin_tag(self, tag, props=None):
        self.log.append(f"begin_tag {tag!r} {props!r}")

    def end_tag(self):
        self.log.append("end_tag")

    def do_tag(self, tag, props=None):
        self.log.append(f"do_tag {tag!r} {props!r}")

    def begin_page(self, page, ctm):
        self.log.append(f"begin_page {mat(page.mediabox)} {page.rotate!r} {mat(ctm)}")

    def end_page(self, page):
        self.log.append("end_page")

    def begin_figure(self, name, bbox, matrix):
        self.log.append(f"begin_figure {name} {items(bbox)} {items(matrix)}")

    def end_figure(self, name):
        self.log.append(f"end_figure {name}")

    def paint_path(self, graphicstate, stroke, fill, evenodd, path):
        self.log.append(f"paint {int(stroke)}{int(fill)}{int(evenodd)} {path_str(path)} {gstate(graphicstate)}")

    def render_image(self, name, stream):
        self.log.append(f"image {name} {stream.objid!r} {stream.attrs!r} {short_sha(stream.rawdata)}")

    def render_string(self, textstate, seq, ncs, graphicstate):
        seq = list(seq)  # like every text device
        ts = textstate
        self.log.append(
            f"str {ts.font!r} {f(ts.fontsize)} {f(ts.charspace)} {f(ts.wordspace)} {f(ts.scaling)} "
            f"{f(ts.leading)} {ts.render!r} {f(ts.rise)} {mat(ts.matrix)} {mat(ts.linematrix)} "
            f"{items(seq)} {cs(ncs)} {gstate(graphicstate)}"
        )


class TextRecorder(PDFTextDevice):
    def __init__(self, rsrcmgr, log):
        super().__init__(rsrcmgr)
        self.log = log

    def render_string(self, textstate, seq, ncs, graphicstate):
        super().render_string(textstate, seq, ncs, graphicstate)
        self.log.append("lm " + mat(textstate.linematrix))

    def render_char(self, matrix, font, fontsize, scaling, rise, cid, ncs, graphicstate):
        adv = font.char_width(cid) * fontsize * scaling
        self.log.append(f"char {mat(matrix)} {cid} {f(fontsize)} {f(scaling)} {f(rise)} {ncs.name} {f(adv)}")
        return adv


def port_compat_str(b: bytes) -> str:
    """The port's make_compat_str for bytes: UTF-8, else repr."""
    try:
        return b.decode("utf-8")
    except UnicodeDecodeError:
        return repr(b)


class CheckedTagExtractor(TagExtractor):
    """Flags properties for which pdfminer's make_compat_str differs from the port's."""

    diverges = False

    def begin_tag(self, tag, props=None):
        if isinstance(props, dict):
            for v in props.values():
                if isinstance(v, bytes) and make_compat_str(v) != port_compat_str(v):
                    CheckedTagExtractor.diverges = True
        super().begin_tag(tag, props)


def open_doc(data: bytes):
    err = None
    for password in PASSWORDS:
        try:
            return PDFDocument(PDFParser(io.BytesIO(data)), password=password), None
        except Exception as e:  # noqa: BLE001
            if err is None:
                err = type(e).__name__
    return None, err


def run(data: bytes, kind: str) -> dict:
    log: list[str] = []
    counter = itertools.count(1)
    pdfinterp.id = lambda o: f"inline{next(counter)}"  # type: ignore[attr-defined]
    doc, err = open_doc(data)
    if doc is None:
        return {"open": err}
    rsrcmgr = PDFResourceManager()
    out = io.BytesIO()
    CheckedTagExtractor.diverges = False
    if kind == "dev":
        device = Recorder(rsrcmgr, log)
    elif kind == "text":
        device = TextRecorder(rsrcmgr, log)
    else:
        device = CheckedTagExtractor(rsrcmgr, out, codec="utf-8")
    interp = PDFPageInterpreter(rsrcmgr, device)
    pages = PDFPage.create_pages(doc)
    for _ in range(MAX_PAGES):
        try:
            page = next(pages)
        except StopIteration:
            break
        except Exception as e:  # noqa: BLE001
            log.append(f"pages error {type(e).__name__}")
            break
        try:
            interp.process_page(page)
            log.append("end")
        except Exception as e:  # noqa: BLE001
            log.append(f"error {type(e).__name__}")
    res: dict = {}
    if kind == "tag":
        b = out.getvalue()
        res["out"] = b.hex() if len(b) <= 4096 else f"sha256:{hashlib.sha256(b).hexdigest()}:{len(b)}"
        if CheckedTagExtractor.diverges:
            res["out_diverges"] = True
    whole = "\n".join(log)
    res["n"] = len(log)
    res["sha"] = hashlib.sha256(whole.encode("utf-8", "surrogatepass")).hexdigest()
    res["log"] = log[:MAX_LINES]
    return res


class Timeout(Exception):
    pass


def on_alarm(signum, frame):
    raise Timeout


def name_for(path: Path, base: Path) -> str:
    return str(path.relative_to(base)).replace("/", "__").removesuffix(".pdf")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", action="append", type=Path)
    ap.add_argument("--out", type=Path, default=OUT)
    args = ap.parse_args()
    sys.setrecursionlimit(1000)
    signal.signal(signal.SIGALRM, on_alarm)
    bases = args.corpus or [UPSTREAM / "samples", ROOT / "tests" / "corpus"]
    args.out.mkdir(parents=True, exist_ok=True)
    total = 0
    for base in bases:
        prefix = "corpus__" if not args.corpus and base.name == "corpus" else ""
        for path in sorted(base.rglob("*.pdf")):
            data = path.read_bytes()
            signal.alarm(TIMEOUT)
            try:
                runs = {k: run(data, k) for k in ("dev", "text", "tag")}
            except Timeout:
                print(f"skipped (over {TIMEOUT}s): {path}", flush=True)
                continue
            finally:
                signal.alarm(0)
            rec = {
                "path": str(path.relative_to(ROOT)) if path.is_relative_to(ROOT) else str(path),
                "runs": runs,
            }
            dest = args.out / f"{prefix}{name_for(path, base)}.json.gz"
            with gzip.open(dest, "wt", encoding="utf-8", errors="surrogatepass") as fp:
                json.dump(rec, fp, separators=(",", ":"), ensure_ascii=True)
            total += dest.stat().st_size
    print(f"wrote {args.out} ({total} bytes)")


if __name__ == "__main__":
    main()
