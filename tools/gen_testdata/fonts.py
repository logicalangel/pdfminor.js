"""Generates the font / cmap differential test data from the reference pdfminer.six.

    uv run python tools/gen_testdata/fonts.py

Writes crates/pdfminer/tests/data/fonts/:

- cmaps.json.gz: every embedded cmap loaded with CMapDB.get_cmap / get_unicode_map, as the
  sha256 of its dump() plus repr and is_vertical, the cids decode() gives for sample byte
  strings, and the exception class for some tricky names.
- <sample>.json.gz: for every font every sample PDF uses, the font spec (references resolved,
  streams decoded and stored without filters) and Python's results for a font rebuilt from that
  spec: fontname, is_vertical, is_multibyte, ascent/descent/width/height, char_width /
  char_disp / to_unichr for cids 0..511 and every cid the PDF draws, decode() of the drawn byte
  strings, string_width, and the CFFFont / TrueTypeFont results for the embedded font files.

The Rust test crates/pdfminer/tests/fonts_difftest.rs rebuilds each font from the dumped spec
and compares everything (floats by repr, exceptions by class name).
"""

import gzip
import hashlib
import io
import json
import random
import sys
from pathlib import Path

from pdfminer.cmapdb import CMapDB
from pdfminer.pdfdevice import PDFDevice
from pdfminer.pdffont import (
    CFFFont,
    PDFCIDFont,
    PDFTrueTypeFont,
    PDFType1Font,
    PDFType3Font,
    TrueTypeFont,
)
from pdfminer.pdfinterp import PDFPageInterpreter, PDFResourceManager
from pdfminer.pdfpage import PDFPage
from pdfminer.pdftypes import PDFObjRef, PDFStream
from pdfminer.psparser import PSKeyword, PSLiteral

ROOT = Path(__file__).resolve().parent.parent.parent
UPSTREAM = ROOT / "upstream" / "pdfminer.six"
SAMPLES = UPSTREAM / "samples"
CMAP_DIR = UPSTREAM / "pdfminer" / "cmap"
OUT = ROOT / "crates" / "pdfminer" / "tests" / "data" / "fonts"

# Streams whose data the font classes read; other streams are kept with empty data.
DATA_KEYS = {"FontFile", "FontFile2", "FontFile3", "ToUnicode", "Encoding"}
# Keys the font classes never read and that can drag in whole pages.
DROP_KEYS = {"Resources", "CharProcs", "Parent", "P"}
STREAM_FILTER_KEYS = {"Filter", "F", "DecodeParms", "DP", "FDecodeParms", "FFilter"}


def exc(e: BaseException) -> str:
    return "!" + type(e).__name__


def enc(x):
    """A Python object as tagged JSON."""
    if x is None or isinstance(x, bool):
        return x
    if isinstance(x, int):
        return x
    if isinstance(x, float):
        return {"f": repr(x)}
    if isinstance(x, bytes):
        return {"bytes": x.hex()}
    if isinstance(x, str):
        return {"str": [ord(c) for c in x]}
    if isinstance(x, PSLiteral):
        if isinstance(x.name, str):
            return {"lit": [ord(c) for c in x.name]}
        return {"litb": x.name.hex()}
    if isinstance(x, PSKeyword):
        return {"kwd": x.name.hex()}
    if isinstance(x, list):
        return {"list": [enc(v) for v in x]}
    if isinstance(x, tuple):
        return {"tuple": [enc(v) for v in x]}
    if isinstance(x, dict):
        return {"dict": [[k, enc(v)] for k, v in x.items()]}
    if isinstance(x, PDFStream):
        return {
            "stream": {
                "attrs": [[k, enc(v)] for k, v in x.attrs.items()],
                # The streams here have no filters; reading the raw data keeps the stream
                # undecoded (its repr shows "raw=" until something calls get_data).
                "data": (x.data if x.rawdata is None else x.rawdata).hex(),
            }
        }
    raise TypeError(f"cannot encode {type(x)}")


class Unresolvable(Exception):
    pass


def convert(x, key=None, chain=()):
    """A deep copy with references resolved (cycles become None), streams decoded."""
    while isinstance(x, PDFObjRef):
        if x.objid in chain:
            return None
        chain = (*chain, x.objid)
        try:
            x = x.resolve()
        except Exception:
            return None
    if isinstance(x, list):
        return [convert(v, None, chain) for v in x]
    if isinstance(x, dict):
        return {k: convert(v, k, chain) for k, v in x.items() if k not in DROP_KEYS}
    if isinstance(x, PDFStream):
        attrs = {k: convert(v, k, chain) for k, v in x.attrs.items() if k not in STREAM_FILTER_KEYS}
        data = b""
        if key in DATA_KEYS:
            try:
                data = x.get_data()
            except Exception as e:
                raise Unresolvable(f"stream data: {e!r}") from e
        return PDFStream(attrs, data)
    return x


CLASSES = {
    "PDFType1Font": PDFType1Font,
    "PDFTrueTypeFont": PDFTrueTypeFont,
    "PDFType3Font": PDFType3Font,
    "PDFCIDFont": PDFCIDFont,
}

# Every font the interpreter creates: (class name, spec); the font object remembers its spec.
CREATED: list = []


def hook(cls):
    orig = cls.__init__

    def __init__(self, rsrcmgr, spec, *args, **kwargs):
        CREATED.append((type(self).__name__, spec))
        self._gen_spec = spec
        orig(self, rsrcmgr, spec, *args, **kwargs)

    cls.__init__ = __init__


for _cls in (PDFType1Font, PDFType3Font, PDFCIDFont):  # PDFTrueTypeFont uses PDFType1Font's
    hook(_cls)


class Recorder(PDFDevice):
    def __init__(self, rsrcmgr):
        super().__init__(rsrcmgr)
        self.strings: dict[int, list[bytes]] = {}
        self.fonts: dict[int, object] = {}

    def render_string(self, textstate, seq, ncs, graphicstate):
        font = textstate.font
        self.fonts[id(font)] = font
        lst = self.strings.setdefault(id(font), [])
        for obj in seq:
            if isinstance(obj, bytes):
                lst.append(obj)


def py_result(f):
    try:
        return f()
    except Exception as e:
        return exc(e)


def float_result(f):
    def g():
        v = f()
        return repr(v)

    return py_result(g)


def font_record(cls_name, spec, strings, ncids=512):
    try:
        cspec = convert(spec)
    except Unresolvable as e:
        print("  skip font:", e, file=sys.stderr)
        return None
    rec = {"class": cls_name, "spec": enc(cspec)}
    cls = CLASSES[cls_name]
    try:
        if cls is PDFCIDFont:
            font = cls(None, cspec, False)
        else:
            font = cls(None, cspec)
    except Exception as e:
        rec["error"] = exc(e)
        return rec
    rec["repr"] = repr(font)
    rec["fontname"] = enc(font.fontname)
    rec["is_vertical"] = font.is_vertical()
    rec["is_multibyte"] = font.is_multibyte()
    rec["ascent"] = float_result(font.get_ascent)
    rec["descent"] = float_result(font.get_descent)
    rec["width"] = float_result(font.get_width)
    rec["height"] = float_result(font.get_height)

    uniq = []
    seen = set()
    for s in strings:
        if s not in seen:
            seen.add(s)
            uniq.append(s)
    uniq = uniq[:300]
    samples = [bytes(range(256)), b"\x00\x01\xff\xfe\x81\x40\x82\xa0\x20", b"", b"\x00"]
    decodes = []
    cids = set(range(ncids))
    for s in samples + uniq:
        try:
            r = list(font.decode(s))
            cids.update(r)
        except Exception as e:
            r = exc(e)
        decodes.append([s.hex(), r])
    rec["decode"] = decodes
    rec["string_width"] = [[s.hex(), float_result(lambda s=s: font.string_width(s))] for s in (samples + uniq)[:20]]
    per_cid = []
    for cid in sorted(cids)[:4000]:
        w = float_result(lambda: font.char_width(cid))
        d = py_result(lambda: enc(font.char_disp(cid)))
        u = py_result(lambda: [ord(c) for c in font.to_unichr(cid)])
        per_cid.append([cid, w, d, u])
    rec["cids"] = per_cid

    descriptor = cspec.get("FontDescriptor") if isinstance(cspec, dict) else None
    if isinstance(descriptor, dict):
        ff3 = descriptor.get("FontFile3")
        if isinstance(ff3, PDFStream):
            rec["cff"] = cff_record(ff3.get_data())
        ff2 = descriptor.get("FontFile2")
        if isinstance(ff2, PDFStream):
            rec["ttf"] = ttf_record(ff2.get_data())
    return rec


def name_enc(n):
    return {"str": n} if isinstance(n, str) else {"bytes": n.hex()}


def cff_record(data):
    try:
        f = CFFFont("x", io.BytesIO(data))
    except Exception as e:
        return exc(e)
    return {
        "nglyphs": f.nglyphs,
        "code2gid": list(f.code2gid.items()),
        "gid2code": list(f.gid2code.items()),
        "name2gid": [[name_enc(k), v] for k, v in f.name2gid.items()],
        "gid2name": [[k, name_enc(v)] for k, v in f.gid2name.items()],
        "top_dict": [[k, [enc(x) for x in v]] for k, v in f.top_dict.items()],
    }


def ttf_record(data):
    f = TrueTypeFont("x", io.BytesIO(data))
    rec = {
        "fonttype": f.fonttype.hex(),
        "tables": [[k.hex(), list(v)] for k, v in f.tables.items()],
    }
    try:
        m = f.create_unicode_map()
        out = io.StringIO()
        m.dump(out)
        rec["unicode_map"] = hashlib.sha256(out.getvalue().encode("utf-8", "surrogatepass")).hexdigest()
    except Exception as e:
        rec["unicode_map"] = exc(e)
    return rec


def dump_hash(m):
    out = io.StringIO()
    m.dump(out)
    return hashlib.sha256(out.getvalue().encode("utf-8", "surrogatepass")).hexdigest()


def cmaps():
    rng = random.Random(1)
    samples = [bytes(range(256)), bytes(rng.randrange(256) for _ in range(2000))]
    samples += [bytes(rng.choice([0x81, 0x82, 0x8E, 0xA1, 0xB0, 0x21, 0x30, 0x00, 0xD8]) for _ in range(500))]
    out = {"samples": [s.hex() for s in samples], "cmaps": [], "unicode_maps": [], "names": []}
    for path in sorted(CMAP_DIR.glob("*.json.gz")):
        name = path.name[: -len(".json.gz")]
        if name.startswith("to-unicode-"):
            base = name[len("to-unicode-") :]
            for v in (False, True):
                m = CMapDB.get_unicode_map(base, v)
                out["unicode_maps"].append(
                    {"name": base, "vertical": v, "repr": repr(m), "is_vertical": m.is_vertical(), "dump": dump_hash(m)}
                )
        else:
            m = CMapDB.get_cmap(name)
            out["cmaps"].append(
                {
                    "name": name,
                    "repr": repr(m),
                    "is_vertical": m.is_vertical(),
                    "dump": dump_hash(m),
                    "decode": [list(m.decode(s)) for s in samples],
                }
            )
    for n in [
        "Identity-H",
        "OneByteIdentityV",
        "sub/../H",
        "H.json.gz/../H",
        "../cmap/H",
        "./UniGB-UCS2-H",
        "/H",
        "H/",
        "a//H",
        "..",
        "",
        "nonexistent",
        "../../pdfminer/cmap/H",
        "to-unicode-Adobe-Japan1",
        "H\0",
    ]:
        r = py_result(lambda: repr(CMapDB.get_cmap(n)))
        u = py_result(lambda: repr(CMapDB.get_unicode_map(n)))
        out["names"].append([n, r if r.startswith("!") else "ok", u if u.startswith("!") else "ok"])
    return out


def fonts_of(pdf: Path):
    """The fonts a PDF creates, with the byte strings drawn with each: {key: (class, spec, strings)}."""
    CREATED.clear()
    rsrcmgr = PDFResourceManager(caching=False)
    dev = Recorder(rsrcmgr)
    interp = PDFPageInterpreter(rsrcmgr, dev)
    # The encrypted samples use the password "foo" (as the upstream tests do).
    password = "foo" if pdf.parent.name == "encryption" else ""
    try:
        with open(pdf, "rb") as fp:
            for page in PDFPage.get_pages(fp, password=password):
                try:
                    interp.process_page(page)
                except Exception as e:
                    print("  page error:", repr(e)[:100], file=sys.stderr)
    except Exception as e:
        print("  pdf error:", repr(e)[:100], file=sys.stderr)
    strings_by_spec: dict[int, list[bytes]] = {}
    for fid, font in dev.fonts.items():
        spec = getattr(font, "_gen_spec", None)
        if spec is not None:
            strings_by_spec.setdefault(id(spec), []).extend(dev.strings.get(fid, []))
    # Fonts are created again for every page (Type0 fonts from a fresh sub-spec): merge equal
    # specs.
    fonts: dict[str, tuple] = {}
    done = set()
    for cls_name, spec in CREATED:
        if id(spec) in done:
            continue
        done.add(id(spec))
        try:
            key = json.dumps([cls_name, enc(convert(spec))], sort_keys=True)
        except Unresolvable as e:
            print("  skip font:", e, file=sys.stderr)
            continue
        entry = fonts.setdefault(key, (cls_name, spec, []))
        entry[2].extend(strings_by_spec.get(id(spec), []))
    return fonts


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    data = cmaps()
    with gzip.open(OUT / "cmaps.json.gz", "wt", encoding="utf-8") as f:
        json.dump(data, f)
    print("cmaps:", len(data["cmaps"]), "unicode maps:", len(data["unicode_maps"]))

    total = 0
    for pdf in sorted(SAMPLES.rglob("*.pdf")):
        rel = pdf.relative_to(SAMPLES).as_posix()
        print(rel, file=sys.stderr)
        records = []
        for cls_name, spec, strings in fonts_of(pdf).values():
            rec = font_record(cls_name, spec, strings)
            if rec is not None:
                records.append(rec)
        total += len(records)
        name = rel.replace("/", "__")[: -len(".pdf")] + ".json.gz"
        with gzip.open(OUT / name, "wt", encoding="utf-8") as f:
            json.dump({"pdf": rel, "fonts": records}, f)
    print("fonts:", total)
    fuzz_main()



# ---------------------------------------------------------------------------------------------
# Fuzz cases: random inputs (fixed seed) that exercise the error paths, with Python's results.


class Timeout(BaseException):
    pass


def with_timeout(f, seconds=0.2):
    """f(), or None when it runs too long (huge ranges that Python itself would grind through)."""
    import signal

    def handler(signum, frame):
        raise Timeout

    old = signal.signal(signal.SIGALRM, handler)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        return f()
    except Timeout:
        return None
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, old)


def random_value(rng, depth=0):
    from pdfminer.psparser import KWD, LIT

    simple = [
        None, True, False, 0, 1, 2, -3, 65, 255, 1000, 0.5, -1.5, 0.0, -0.0, 1e300, 250.25,
        b"", b"Adobe", b"Identity", b"\x00\x41", b"12",
        LIT("Identity-H"), LIT("Identity-V"), LIT("WinAnsiEncoding"), LIT("MacRomanEncoding"),
        LIT("Helvetica"), LIT("Courier-Bold"), LIT("uni0041"), LIT("foo"), LIT(b"\xff\xfe"),
        LIT("UniJIS-UCS2-H"), LIT("TrueType"), LIT("OneByteIdentityH"), LIT("DLIdent-V"),
        KWD(b"foo"),
    ]
    r = rng.random()
    if depth > 1 or r < 0.6:
        return rng.choice(simple)
    if r < 0.8:
        return [random_value(rng, depth + 1) for _ in range(rng.choice([0, 1, 2, 3, 4, 6]))]
    if r < 0.93:
        keys = ["CMapName", "BaseEncoding", "Differences", "Registry", "Ordering", "Length1", "FontBBox"]
        return {rng.choice(keys): random_value(rng, depth + 1) for _ in range(rng.choice([0, 1, 2]))}
    return PDFStream({"Length1": rng.choice([0, 5, -3, 100])}, bytes(rng.randrange(256) for _ in range(20)))


SPEC_KEYS = [
    "Encoding", "ToUnicode", "W", "W2", "DW", "DW2", "FontDescriptor", "FontBBox", "FontMatrix",
    "Widths", "FirstChar", "BaseFont", "Subtype", "CIDSystemInfo", "MissingWidth", "Ascent",
    "Descent", "FontName", "Flags", "Leading", "ItalicAngle", "FontFile", "FontFile2",
]


def mutate_spec(rng, spec):
    """A (decoded, so fresh) spec with a few random changes."""
    for _ in range(rng.choice([1, 1, 2, 3])):
        containers = []

        def walk(x):
            if isinstance(x, dict):
                containers.append(x)
                for v in x.values():
                    walk(v)
            elif isinstance(x, list):
                containers.append(x)
                for v in x:
                    walk(v)

        walk(spec)
        c = rng.choice(containers)
        op = rng.random()
        if isinstance(c, dict):
            if c and op < 0.5:
                c[rng.choice(list(c))] = random_value(rng)
            elif c and op < 0.7:
                del c[rng.choice(list(c))]
            else:
                c[rng.choice(SPEC_KEYS)] = random_value(rng)
        else:
            if c and op < 0.6:
                c[rng.randrange(len(c))] = random_value(rng)
            elif c and op < 0.8:
                del c[rng.randrange(len(c))]
            else:
                c.insert(rng.randrange(len(c) + 1), random_value(rng))
    return spec


CMAP_TOKENS = [
    "<00>", "<01>", "<0041>", "<00410042>", "<FFFF>", "<FFFFFFFF>", "<>", "<D800>", "<0000FFFE>",
    "[", "]", "/A", "/uni0041", "/uniform", "/space", "/u1F600", "12", "-1", "0", "65", "1.5",
    "def", "beginbfrange", "endbfrange", "beginbfchar", "endbfchar", "begincidrange",
    "endcidrange", "begincidchar", "endcidchar", "usecmap", "/H", "/Identity-H", "begincmap",
    "endcmap", "(abc)", "<<", ">>", "begincodespacerange", "endcodespacerange", "true", "null",
]

TOUNICODE = b"""/CIDInit /ProcSet findresource begin 12 dict begin begincmap
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
/CMapName /Adobe-Identity-UCS def /CMapType 2 def
1 begincodespacerange <0000> <FFFF> endcodespacerange
3 beginbfchar <0003> <0020> <0004> <00410042> <0005> <D83DDE00> endbfchar
3 beginbfrange <0010> <0012> <0061> <0020> <0022> [<0031> /two <0033>] <00FE> <0101> <00FE> endbfrange
1 begincidrange <0100> <0101> 50 endcidrange
2 begincidchar 7 <0063> <0064> 8 endcidchar
endcmap CMapName currentdict /CMap defineresource pop end end"""


def mutate_tokens(rng, data, tokens):
    toks = data.split()
    for _ in range(rng.choice([1, 2, 3, 5])):
        op = rng.random()
        i = rng.randrange(len(toks) + 1)
        if op < 0.3 and toks:
            del toks[min(i, len(toks) - 1)]
        elif op < 0.6:
            toks.insert(i, rng.choice(tokens).encode())
        elif toks:
            toks[min(i, len(toks) - 1)] = rng.choice(tokens).encode()
    return b" ".join(toks)


def unicode_map_result(data):
    from pdfminer.cmapdb import CMapParser, FileUnicodeMap

    def run():
        m = FileUnicodeMap()
        try:
            CMapParser(m, io.BytesIO(data)).run()
        except Exception as e:
            return exc(e)
        return dump_hash(m)

    return with_timeout(run)


TYPE1_HEADER = b"""%!PS-AdobeFont-1.0: Test 001.000
/FontName /Test def
/Encoding 256 array
0 1 255 {1 index exch /.notdef put} for
dup 65 /A put
dup 66 /uni0042 put
dup 67 /foo put
dup 68 /Lcommaaccent_uni20AC0308 put
readonly def
currentdict end
currentfile eexec"""

TYPE1_TOKENS = [
    "dup", "put", "/uniform", "/u1F600", "/uFFFFFF", "/A", "/space", "{", "}", "[", "]", "1.5",
    "65", "-1", "true", "/uni0x41", "/uniD800", "def", "(x)", "<41>", "/.notdef", "/a_b",
]


def type1_result(data):
    from pdfminer.pdffont import Type1FontHeaderParser

    def run():
        try:
            e = Type1FontHeaderParser(io.BytesIO(data)).get_encoding()
        except Exception as ex:
            return exc(ex)
        return [[k, [ord(c) for c in v]] for k, v in sorted(e.items())]

    return with_timeout(run)


NAME_ALPHABET = ["u", "n", "i", "0", "4", "1", "A", "f", "F", "_", ".", "x", " ", "٣", "-", "+",
                 "　", "D", "8", "G", "C", "a"]


def name_result(name):
    from pdfminer.encodingdb import name2unicode

    try:
        return [ord(c) for c in name2unicode(name)]
    except Exception as e:
        return exc(e)


def sfnt(tables):
    """A minimal sfnt file with the given {tag: data} tables."""
    import struct

    n = len(tables)
    out = struct.pack(">LHHHH", 0x00010000, n, 0, 0, 0)
    offset = 12 + 16 * n
    body = b""
    for tag, data in tables.items():
        out += struct.pack(">4sLLL", tag, 0, offset + len(body), len(data))
        body += data
    return out + body


def cmap_table(subtables):
    import struct

    out = struct.pack(">HH", 0, len(subtables))
    offset = 4 + 8 * len(subtables)
    body = b""
    for pid, eid, data in subtables:
        out += struct.pack(">HHL", pid, eid, offset + len(body))
        body += data
    return out + body


def cmap_subtables(rng):
    import struct

    f0 = struct.pack(">HHH", 0, 262, 0) + bytes(rng.randrange(256) for _ in range(256))
    keys = [0] * 256
    keys[0x81] = 8
    f2 = struct.pack(">HHH", 2, 0, 0) + struct.pack(">256H", *keys)
    f2 += struct.pack(">HHhH", 0, 0, 0, 0) + struct.pack(">HHhH", 0x40, 3, 5, 2) + struct.pack(">3H", 1, 0, 7)
    segs = [(0x20, 0x7E, -29, 0), (0x100, 0x102, 0, 4), (0xFFFF, 0xFFFF, 1, 0)]
    segcount = len(segs)
    f4 = struct.pack(">HHH", 4, 0, 0) + struct.pack(">HHHH", segcount * 2, 0, 0, 0)
    f4 += struct.pack(f">{segcount}H", *[s[1] for s in segs]) + b"\0\0"
    f4 += struct.pack(f">{segcount}H", *[s[0] for s in segs])
    f4 += struct.pack(f">{segcount}h", *[s[2] for s in segs])
    f4 += struct.pack(f">{segcount}H", *[s[3] for s in segs]) + struct.pack(">3H", 9, 10, 11)
    f6 = struct.pack(">HHHHH", 6, 0, 0, 0x30, 4) + struct.pack(">4H", 1, 2, 3, 4)
    f10 = struct.pack(">HHII", 10, 0, 0, 0) + struct.pack(">II", 0x1F600, 2) + struct.pack(">2H", 5, 6)
    f12 = struct.pack(">HHII", 12, 0, 0, 0) + struct.pack(">I", 2)
    f12 += struct.pack(">III", 0x41, 0x43, 7) + struct.pack(">III", 0x10FFFE, 0x110001, 9)
    f14 = struct.pack(">HI", 14, 10) + struct.pack(">I", 0)
    return {"f0": f0, "f2": f2, "f4": f4, "f6": f6, "f10": f10, "f12": f12, "f14": f14}


def random_ttf(rng):
    subs = cmap_subtables(rng)
    chosen = rng.sample(sorted(subs), rng.choice([1, 1, 2, 3]))
    plats = [(0, 3), (3, 1), (3, 10), (1, 0), (3, 0), (0, 5)]
    subtables = [(*rng.choice(plats), subs[k]) for k in chosen]
    tables = {b"cmap": cmap_table(subtables)}
    if rng.random() < 0.3:
        tables = {b"head": b"\0" * 54, **tables}
    return sfnt(tables)


def mutate_bytes(rng, data, region=None):
    data = bytearray(data)
    lo, hi = region or (0, len(data))
    hi = min(hi, len(data))
    for _ in range(rng.choice([1, 2, 4])):
        if hi <= lo:
            break
        if rng.random() < 0.15:
            del data[rng.randrange(lo, hi):]
            break
        data[rng.randrange(lo, hi)] = rng.choice([0, 1, 0xFF, 0x7F, 0x80, rng.randrange(256)])
    return bytes(data)


def cff_index(items):
    import struct

    offs = [1]
    for it in items:
        offs.append(offs[-1] + len(it))
    return struct.pack(">HB", len(items), 4) + b"".join(struct.pack(">L", o) for o in offs) + b"".join(items)


def cff_int(v):
    import struct

    return b"\x1d" + struct.pack(">l", v)


def random_cff(rng):
    import struct

    nglyphs = rng.choice([1, 3, 5])
    strings = [b"custom1", b"custom2"]
    charstrings = cff_index([b"\x0e"] * nglyphs)
    if rng.random() < 0.5:
        encoding = bytes([0, nglyphs - 1]) + bytes(range(1, nglyphs))
    else:
        encoding = bytes([1, 1, 1, nglyphs - 2 if nglyphs > 1 else 0])
    if rng.random() < 0.5:
        sids = [rng.choice([1, 2, 34, 390, 391, 392, 393]) for _ in range(nglyphs - 1)]
        charset = b"\x00" + struct.pack(f">{nglyphs - 1}H", *sids)
    else:
        charset = bytes([1, 2, 1, 1, 5, 0])
    header = bytes([1, 0, 4, 4])
    name_index = cff_index([b"Test"])
    top_len = len(cff_index([b"\0" * 18]))
    rest_start = len(header) + len(name_index) + top_len
    string_index = cff_index(strings)
    gsubr = cff_index([])
    cs_pos = rest_start + len(string_index) + len(gsubr)
    enc_pos = cs_pos + len(charstrings)
    charset_pos = enc_pos + len(encoding)
    top = cff_int(charset_pos) + b"\x0f" + cff_int(enc_pos) + b"\x10" + cff_int(cs_pos) + b"\x11"
    data = header + name_index + cff_index([top]) + string_index + gsubr + charstrings + encoding + charset
    if rng.random() < 0.3:
        # a real-number operand
        real = b"\x1e\x1a\x5f\x10" if rng.random() < 0.5 else b"\x1e\xd0\x10"
        data = data.replace(b"\x1d" + struct.pack(">l", enc_pos) + b"\x10", real, 1)
    return data


def dec_value(j):
    """The inverse of enc()."""
    from pdfminer.psparser import KWD, LIT

    if j is None or isinstance(j, (bool, int)):
        return j
    ((tag, v),) = j.items()
    if tag == "f":
        return float(v)
    if tag == "bytes":
        return bytes.fromhex(v)
    if tag == "str":
        return "".join(chr(c) for c in v)
    if tag == "lit":
        return LIT("".join(chr(c) for c in v))
    if tag == "litb":
        return LIT(bytes.fromhex(v))
    if tag == "kwd":
        return KWD(bytes.fromhex(v))
    if tag == "list":
        return [dec_value(x) for x in v]
    if tag == "tuple":
        return tuple(dec_value(x) for x in v)
    if tag == "dict":
        return {k: dec_value(x) for k, x in v}
    if tag == "stream":
        return PDFStream({k: dec_value(x) for k, x in v["attrs"]}, bytes.fromhex(v["data"]))
    raise ValueError(tag)


def fuzz_main():
    rng = random.Random(20260107)
    out = {"specs": [], "tounicode": [], "type1": [], "names": [], "ttf": [], "cff": []}

    # Mutated font specs, from every sample font.
    base_specs = []
    for path in sorted(OUT.glob("*.json.gz")):
        if path.name in ("cmaps.json.gz", "fuzz.json.gz"):
            continue
        with gzip.open(path, "rt", encoding="utf-8") as f:
            base_specs.extend(json.load(f)["fonts"])
    def small(j):
        """The spec without big stream data (the font files are covered elsewhere)."""
        if isinstance(j, dict) and "stream" in j and len(j["stream"]["data"]) > 8192:
            return {"stream": {"attrs": j["stream"]["attrs"], "data": j["stream"]["data"][:2048]}}
        if isinstance(j, dict):
            return {k: small(v) for k, v in j.items()}
        if isinstance(j, list):
            return [small(v) for v in j]
        return j

    for _ in range(1200):
        rec = rng.choice(base_specs)
        cls_name = rec["class"]
        if rng.random() < 0.15:
            cls_name = rng.choice(sorted(CLASSES))
        spec = mutate_spec(rng, dec_value(small(rec["spec"])))
        r = with_timeout(lambda: font_record(cls_name, spec, [], ncids=40), 1)
        if r is not None:
            out["specs"].append(r)

    # ToUnicode CMaps.
    sources = [TOUNICODE]
    for rec in base_specs:
        for kv in rec["spec"].get("dict", []):
            if kv[0] == "ToUnicode" and isinstance(kv[1], dict) and "stream" in kv[1]:
                sources.append(bytes.fromhex(kv[1]["stream"]["data"]))
    for _ in range(800):
        data = mutate_tokens(rng, rng.choice(sources), CMAP_TOKENS)
        r = unicode_map_result(data)
        if r is not None:
            out["tounicode"].append([data.hex(), r])

    for _ in range(400):
        data = mutate_tokens(rng, TYPE1_HEADER, TYPE1_TOKENS)
        r = type1_result(data)
        if r is not None:
            out["type1"].append([data.hex(), r])

    for _ in range(1500):
        prefix = rng.choice(["uni", "u", "", "uni", "u"])
        name = prefix + "".join(rng.choice(NAME_ALPHABET) for _ in range(rng.randrange(0, 11)))
        out["names"].append([[ord(c) for c in name], name_result(name)])

    ttfs = []
    for rec in base_specs:
        for kv in rec["spec"].get("dict", []):
            if kv[0] == "FontDescriptor" and isinstance(kv[1], dict) and "dict" in kv[1]:
                for k2 in kv[1]["dict"]:
                    if k2[0] == "FontFile2" and isinstance(k2[1], dict) and "stream" in k2[1]:
                        data = bytes.fromhex(k2[1]["stream"]["data"])
                        # Keep only the cmap table, where the parsing happens.
                        f = TrueTypeFont("x", io.BytesIO(data))
                        if b"cmap" in f.tables:
                            off, length = f.tables[b"cmap"]
                            ttfs.append(sfnt({b"cmap": data[off : off + length]}))
    for _ in range(600):
        if ttfs and rng.random() < 0.4:
            data = mutate_bytes(rng, rng.choice(ttfs), (12, 12 + 2000))
        else:
            data = random_ttf(rng)
            if rng.random() < 0.7:
                data = mutate_bytes(rng, data, (12, len(data)))
        r = with_timeout(lambda: ttf_record(data))
        if r is not None:
            out["ttf"].append([data.hex(), r])

    for _ in range(600):
        data = random_cff(rng)
        if rng.random() < 0.7:
            data = mutate_bytes(rng, data)
        r = with_timeout(lambda: cff_record(data))
        if r is not None:
            out["cff"].append([data.hex(), r])

    with gzip.open(OUT / "fuzz.json.gz", "wt", encoding="utf-8") as f:
        json.dump(out, f)
    print("fuzz:", {k: len(v) for k, v in out.items()})

if __name__ == "__main__":
    main()
