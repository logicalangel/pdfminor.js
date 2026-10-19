"""Writes the hand-made PDFs in tests/corpus/: small files that each aim at a pdfminer behaviour
real-world PDFs rarely reach (lone surrogates in text, infinite coordinates, operands of the
wrong type, the shared graphic state, vertical and Type3 fonts, broken xrefs, ...).

    uv run python tools/make_corpus.py

The files are committed; rerun this only after changing a case. tools/difftest.py picks up
everything under tests/corpus/.
"""

import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "tests" / "corpus"

HELVETICA = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"


class PDF:
    """A tiny PDF writer: objects are bytes; streams get /Length; the xref is computed."""

    def __init__(self) -> None:
        self.objs: list[bytes] = []

    def add(self, body: bytes) -> int:
        self.objs.append(body)
        return len(self.objs)

    def stream(self, dict_body: bytes, data: bytes, compress: bool = False) -> int:
        if compress:
            data = zlib.compress(data)
            dict_body += b" /Filter /FlateDecode"
        return self.add(b"<< " + dict_body + b" /Length %d >>\nstream\n" % len(data) + data + b"\nendstream")

    def build(self, root: int, info: int | None = None, xref: str = "table", tail: bytes = b"") -> bytes:
        out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
        offsets = []
        for i, body in enumerate(self.objs, 1):
            offsets.append(len(out))
            out += b"%d 0 obj\n" % i + body + b"\nendobj\n"
        startxref = len(out)
        trailer = b"<< /Size %d /Root %d 0 R" % (len(self.objs) + 1, root)
        if info:
            trailer += b" /Info %d 0 R" % info
        trailer += b" >>"
        if xref == "table":
            out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(self.objs) + 1)
            for off in offsets:
                out += b"%010d 00000 n \n" % off
            out += b"trailer\n" + trailer + b"\nstartxref\n%d\n%%%%EOF\n" % startxref
        elif xref == "broken":
            # offsets all wrong: forces the fallback xref scan
            out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(self.objs) + 1)
            for off in offsets:
                out += b"%010d 00000 n \n" % (off + 7)
            out += b"trailer\n" + trailer + b"\nstartxref\n%d\n%%%%EOF\n" % startxref
        elif xref == "none":
            out += b"trailer\n" + trailer + b"\n%%EOF\n"
        out += tail
        return bytes(out)


def simple_page(pdf: PDF, contents: list[bytes], resources: bytes, mediabox: bytes = b"[0 0 612 792]",
                extra: bytes = b"") -> int:
    """Adds pages tree with one page; returns the catalog id."""
    content_ids = [pdf.stream(b"", c) for c in contents]
    res = pdf.add(resources)
    pages_id = len(pdf.objs) + 2
    ckids = b" ".join(b"%d 0 R" % i for i in content_ids)
    page = pdf.add(
        b"<< /Type /Page /Parent %d 0 R /MediaBox %s /Resources %d 0 R /Contents [%s] %s >>"
        % (pages_id, mediabox, res, ckids, extra)
    )
    pdf.add(b"<< /Type /Pages /Kids [%d 0 R] /Count 1 >>" % page)
    return pdf.add(b"<< /Type /Catalog /Pages %d 0 R >>" % pages_id)


def helvetica_page(content: bytes, **kw) -> bytes:
    pdf = PDF()
    font = pdf.add(HELVETICA)
    root = simple_page(pdf, [content], b"<< /Font << /F1 %d 0 R >> >>" % font, **kw)
    return pdf.build(root)


def identity_font(pdf: PDF, vertical: bool = False, tounicode: bytes = b"/Identity-H", extra: bytes = b"") -> int:
    desc = pdf.add(b"<< /Type /FontDescriptor /FontName /TestCID /Flags 4 /FontBBox [0 -200 1000 900] "
                   b"/Ascent 800 /Descent -200 /ItalicAngle 0 /CapHeight 700 /StemV 80 >>")
    w = b"/W2 [1 [1000 500 880] 100 200 900 400 870]" if vertical else b"/W [1 [500 600 700] 100 200 450]"
    cid = pdf.add(b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /TestCID "
                  b"/CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> "
                  b"/FontDescriptor %d 0 R %s %s >>" % (desc, w, extra))
    enc = b"/Identity-V" if vertical else b"/Identity-H"
    return pdf.add(b"<< /Type /Font /Subtype /Type0 /BaseFont /TestCID /Encoding %s "
                   b"/DescendantFonts [%d 0 R] /ToUnicode %s >>" % (enc, cid, tounicode))


def cases() -> dict[str, bytes]:
    c: dict[str, bytes] = {}

    # Identity unicode map: chr(cid) of surrogate code points ends up in the text.
    pdf = PDF()
    f = identity_font(pdf)
    root = simple_page(pdf, [b"BT /F1 12 Tf 72 700 Td <D800DC000041DFFF0020D83DDE00> Tj ET"],
                       b"<< /Font << /F1 %d 0 R >> >>" % f)
    c["surrogates-identity.pdf"] = pdf.build(root)

    # Vertical CID font with W2/DW2.
    pdf = PDF()
    f = identity_font(pdf, vertical=True, tounicode=b"/Identity-V", extra=b"/DW2 [880 -1000]")
    root = simple_page(pdf, [b"BT /F1 20 Tf 300 700 Td <0001000200030064> Tj 0 -40 Td <0041 0042> Tj ET"],
                       b"<< /Font << /F1 %d 0 R >> >>" % f)
    c["vertical-cid.pdf"] = pdf.build(root)

    # Numbers that overflow to inf (a 400-digit float token) and make NaN downstream.
    big = b"1" + b"0" * 400 + b".0"
    c["inf-coordinates.pdf"] = helvetica_page(
        b"BT /F1 12 Tf 72 700 Td (before) Tj ET\n"
        b"BT /F1 12 Tf " + big + b" 700 Td (inf x) Tj ET\n"
        b"BT /F1 " + big + b" Tf 72 600 Td (inf size) Tj ET\n"
        b"q " + big + b" 0 0 1 0 0 cm BT /F1 12 Tf 72 500 Td (inf ctm) Tj ET Q\n"
        b"q 0 0 0 0 0 0 cm BT /F1 12 Tf 72 400 Td (zero ctm) Tj ET Q\n"
        b"72 300 m " + big + b" 300 l S\n"
        b"BT /F1 12 Tf 72 200 Td (after) Tj ET"
    )

    # Operands of the wrong type for many operators.
    c["wrong-operands.pdf"] = helvetica_page(
        b"BT /F1 12 Tf 72 700 Td (ok) Tj ET\n"
        b"BT /X /Y Td (a) Tf /F1 (12) Tf 72 650 Td (string size) Tj ET\n"
        b"BT /F1 12 Tf [1 2] 0 0 1 72 600 Tm (array matrix) Tj ET\n"
        b"BT /F1 12 Tf 72 550 Td /Name Tc (literal Tc) Tj (x) Tw (string Tw) Tj ET\n"
        b"BT /F1 12 Tf 72 500 Td 0 Tz (zero scaling) Tj -50 Tz (negative scaling) Tj ET\n"
        b"BT /F1 0 Tf 72 450 Td (zero size) Tj /F1 -12 Tf (negative size) Tj ET\n"
        b"BT /F1 12 Tf 72 400 Td [(A) /lit (B) -200 (C) [1] (D)] TJ ET\n"
        b"BT /Missing 12 Tf 72 350 Td (missing font) Tj ET\n"
        b"q Q Q Q BT /F1 12 Tf 72 300 Td (extra Q) Tj ET\n"
        b"ET ET BT BT /F1 12 Tf 72 250 Td (nested BT) Tj ET\n"
        b"/a /b /c d (x) 1 2 3 4 5 6 7 8 9 re (y) w /z g 1 2 rg (p) (q) (r) k\n"
        b"BT /F1 12 Tf 72 200 Td 2 Tr (render 2) Tj 1 0 0 1 0 0 Tm T* (T*) Tj ' \" ET\n"
        b"BT /F1 12 Tf 72 150 Td 10 20 (a) \" (b) ' 5 TL T* (c) Tj ET"
    )

    # The shared graphic state: colors set after drawing, inside and around q/Q.
    c["graphicstate-aliasing.pdf"] = helvetica_page(
        b"BT /F1 12 Tf 72 700 Td (black) Tj ET\n"
        b"1 0 0 rg BT /F1 12 Tf 72 680 Td (red then green) Tj ET 0 1 0 rg\n"
        b"q 0 0 1 rg BT /F1 12 Tf 72 660 Td (blue in q) Tj ET 0.5 g Q\n"
        b"BT /F1 12 Tf 72 640 Td (after Q) Tj ET 0 0 0 1 k\n"
        b"q BT /F1 12 Tf 72 620 Td (before q copy) Tj ET q 1 1 0 rg Q Q\n"
        b"/DeviceRGB cs 0.2 0.4 0.6 sc BT /F1 12 Tf 72 600 Td (sc) Tj ET\n"
        b"/Pattern cs /P1 scn BT /F1 12 Tf 72 580 Td (pattern) Tj ET\n"
        b"3 w 72 560 m 200 560 l S 0.25 0.5 0.75 RG 1 0 0 1 0 0 cm 72 540 m 200 540 l S\n"
        b"[3 2] 1 d 72 520 m 200 520 l S 1 J 2 j 4 M /RI ri 0.5 i\n"
        b"BT /F1 12 Tf 72 500 Td 3 Tr (invisible) Tj ET"
    )

    # Paths: rectangles, curves, closed and unclosed lines, multiple subpaths.
    c["paths.pdf"] = helvetica_page(
        b"72 72 100 50 re S 72 200 m 172 200 l 172 250 l 72 250 l h f\n"
        b"72 300 m 100 350 150 350 172 300 c S 72 400 m 150 450 172 400 v 200 380 y h B\n"
        b"72 500 m 172 500 l 72 520 m 172 520 l S 300 300 m 300 300 l S\n"
        b"300 100 m h S 300 150 m 350 150 l 350 200 l 300 200 l 300 150 l S\n"
        b"1 0 0 -1 0 792 cm 400 100 50 50 re f* 400 200 50 50 re B* n W n"
    )

    # Type3 font with Differences and a FontMatrix.
    pdf = PDF()
    proc = pdf.stream(b"", b"1000 0 0 0 750 750 d1 0 0 750 750 re f")
    font = pdf.add(
        b"<< /Type /Font /Subtype /Type3 /FontBBox [0 0 750 750] /FontMatrix [0.001 0 0 0.001 0 0] "
        b"/CharProcs << /square %d 0 R >> /Encoding << /Type /Encoding /Differences [65 /square /uni0042 /u1F600 /g12] >> "
        b"/FirstChar 65 /LastChar 68 /Widths [1000 500 250 125] >>" % proc
    )
    root = simple_page(pdf, [b"BT /F1 24 Tf 72 700 Td (ABCD) Tj ET"], b"<< /Font << /F1 %d 0 R >> >>" % font)
    c["type3-font.pdf"] = pdf.build(root)

    # Standard 14 fonts and built-in encodings, including chars without a mapping.
    pdf = PDF()
    fonts = [
        pdf.add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Times-Roman >>"),
        pdf.add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier /Encoding /MacRomanEncoding >>"),
        pdf.add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Symbol >>"),
        pdf.add(b"<< /Type /Font /Subtype /TrueType /BaseFont /Arial,Bold >>"),
        pdf.add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Unknown-Font /FirstChar 32 /Widths [250 300] >>"),
        pdf.add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << /BaseEncoding /PDFDocEncoding "
                b"/Differences [128 /Euro /bullet /notaglyph 200 /space] >> >>"),
    ]
    res = b"<< /Font << " + b" ".join(b"/F%d %d 0 R" % (i, f) for i, f in enumerate(fonts)) + b" >> >>"
    content = b"".join(
        b"BT /F%d 11 Tf 72 %d Td (Hello \\200\\201\\202\\310\\377 \\(x\\)) Tj ET\n" % (i, 700 - 30 * i) for i in range(len(fonts))
    )
    root = simple_page(pdf, [content], res)
    c["builtin-encodings.pdf"] = pdf.build(root)

    # ToUnicode CMap with ranges, arrays, multi-char targets and odd code space.
    cmap = (
        b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n"
        b"/CMapName /Test def /CMapType 2 def\n"
        b"1 begincodespacerange <00> <FF> endcodespacerange\n"
        b"3 beginbfchar <41> <0058> <42> <00660069> <43> /eacute endbfchar\n"
        b"2 beginbfrange <61> <63> <0061> <64> <66> [<0031> <0032> <D835DC00>] endbfrange\n"
        b"1 beginbfchar <44> <D800> endbfchar\n"
        b"endcmap CMapName currentdict /CMap defineresource pop end end"
    )
    pdf = PDF()
    tu = pdf.stream(b"", cmap)
    font = pdf.add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /ToUnicode %d 0 R >>" % tu)
    root = simple_page(pdf, [b"BT /F1 12 Tf 72 700 Td (ABCDabcdefZ) Tj ET"], b"<< /Font << /F1 %d 0 R >> >>" % font)
    c["tounicode-cmap.pdf"] = pdf.build(root)

    # Inline images (plain and ASCII85) and an image XObject in a form inside a form.
    pdf = PDF()
    img = pdf.stream(b"/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8",
                     b"\x00\xff\xff\x00")
    inner = pdf.stream(b"/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Matrix [2 0 0 2 10 10] "
                       b"/Resources << /XObject << /Im1 %d 0 R >> >>" % img, b"q 50 0 0 50 0 0 cm /Im1 Do Q")
    outer = pdf.stream(b"/Type /XObject /Subtype /Form /BBox [0 0 300 300] "
                       b"/Resources << /XObject << /Fm2 %d 0 R >> >>" % inner, b"/Fm2 Do")
    content = (b"q 20 0 0 20 72 600 cm BI /W 2 /H 2 /CS /G /BPC 8 ID \x00\xff\xff\x00 EI Q\n"
               b"q 20 0 0 20 72 500 cm BI /W 1 /H 1 /CS /RGB /BPC 8 /F /A85 ID z~> EI Q\n"
               b"q 1 0 0 1 100 100 cm /Fm1 Do Q /Missing Do")
    root = simple_page(pdf, [content], b"<< /XObject << /Fm1 %d 0 R >> >>" % outer)
    c["images-forms.pdf"] = pdf.build(root)

    # A form XObject that draws itself (recursion), and one with a broken matrix.
    pdf = PDF()
    me = len(pdf.objs) + 1
    pdf.stream(b"/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /Me %d 0 R >> >>" % me,
               b"0 0 5 5 re f /Me Do")
    bad = pdf.stream(b"/Type /XObject /Subtype /Form /BBox [0 0 10] /Matrix [1 0 (x) 1 0 0]", b"0 0 5 5 re f")
    root = simple_page(pdf, [b"/Me Do /Bad Do"], b"<< /XObject << /Me %d 0 R /Bad %d 0 R >> >>" % (me, bad))
    c["recursive-form.pdf"] = pdf.build(root)

    # A token split across two content streams (issue #1157) and an empty stream.
    pdf = PDF()
    font = pdf.add(HELVETICA)
    root = simple_page(pdf, [b"BT /F1 12 Tf 72 700 Td (split) T", b"j ET", b"", b"BT /F1 12 Tf 72 680 Td (b) Tj ET"],
                       b"<< /Font << /F1 %d 0 R >> >>" % font)
    c["split-token.pdf"] = pdf.build(root)

    # Broken xref offsets (fallback scan), and no xref at all.
    pdf = PDF()
    font = pdf.add(HELVETICA)
    root = simple_page(pdf, [b"BT /F1 12 Tf 72 700 Td (broken xref) Tj ET"], b"<< /Font << /F1 %d 0 R >> >>" % font)
    c["broken-xref.pdf"] = pdf.build(root, xref="broken")
    c["no-xref.pdf"] = pdf.build(root, xref="none")

    # Rotations, odd boxes, page attributes inherited from the Pages node.
    pdf = PDF()
    font = pdf.add(HELVETICA)
    contents = [pdf.stream(b"", b"BT /F1 12 Tf 72 700 Td (rotate %d) Tj ET" % r) for r in (0, 90, 180, 270, 45)]
    pages_id = len(pdf.objs) + 7 + 1  # after the 7 page objects below
    kids = []
    for r, cid in zip((0, 90, 180, 270, 45), contents, strict=True):
        kids.append(pdf.add(b"<< /Type /Page /Parent %d 0 R /Rotate %d /Contents %d 0 R >>" % (pages_id, r, cid)))
    kids.append(pdf.add(b"<< /Type /Page /Parent %d 0 R /Rotate 90.0 /MediaBox [0 0 100] /Contents %d 0 R >>"
                        % (pages_id, contents[0])))
    kids.append(pdf.add(b"<< /Type /Page /Parent %d 0 R /CropBox [10 10 300 300] /MediaBox [612 792 0 0] "
                        b"/Contents %d 0 R >>" % (pages_id, contents[0])))
    assert len(pdf.objs) + 1 == pages_id
    pdf.add(b"<< /Type /Pages /Kids [%s] /Count %d /MediaBox [0 0 612 792] /Resources << /Font << /F1 %d 0 R >> >> >>"
            % (b" ".join(b"%d 0 R" % k for k in kids), len(kids), font))
    root = pdf.add(b"<< /Type /Catalog /Pages %d 0 R /PageLabels << /Nums [0 << /S /r >> 2 << /S /A /P (p-) /St 3 >>] >> >>"
                   % pages_id)
    c["rotations-labels.pdf"] = pdf.build(root)

    # Text matrices: mirrored, rotated, skewed, tiny and huge; char/word spacing; rise.
    c["text-matrices.pdf"] = helvetica_page(
        b"BT /F1 12 Tf -1 0 0 1 300 700 Tm (mirrored) Tj ET\n"
        b"BT /F1 12 Tf 0 1 -1 0 300 600 Tm (rotated 90) Tj ET\n"
        b"BT /F1 12 Tf 1 0.5 0.3 1 72 500 Tm (skewed) Tj ET\n"
        b"BT /F1 12 Tf 0.0001 0 0 0.0001 72 450 Tm (tiny) Tj ET\n"
        b"BT /F1 12 Tf 2 Tc 5 Tw 150 Tz 3 Ts 72 400 Td (spaced out words) Tj ET\n"
        b"BT /F1 12 Tf 72 350 Td [(Kern) -500 (ing) 300 (back)] TJ ET\n"
        b"BT /F1 12 Tf 1 0 0 -1 72 300 Tm (upside down) Tj ET\n"
        b"BT /F1 12 Tf 72 250 Td (\\0\\1\\2\\t\\r\\n\\f\\b) Tj ET"
    )

    # Many overlapping lines and boxes to exercise group_textboxes ties.
    lines = b"".join(b"BT /F1 10 Tf %d %d Td (word%d) Tj ET\n" % (72 + (i % 5) * 100, 700 - (i // 5) * 12, i) for i in range(60))
    lines += b"".join(b"BT /F1 10 Tf 72 100 Td (same%d) Tj ET\n" % i for i in range(5))
    c["many-boxes.pdf"] = helvetica_page(lines)

    return c


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for name, data in cases().items():
        (OUT / name).write_bytes(data)
        print(f"wrote tests/corpus/{name} ({len(data)} bytes)")


if __name__ == "__main__":
    main()
