"""Small hand-made PDFs for the edge cases of pdfdocument/pdfpage that the samples miss.

`write_all(dir)` writes them; `pdfdocument.py` dumps them like the samples.
"""

import pathlib


def build(objs, trailer, *, xref=True, header=b"%PDF-1.4\n", extra_tail=b""):
    """A PDF with the given objects (`{objid: body}`), an xref table and `trailer` (a dict body)."""
    out = bytearray(header)
    offsets = {}
    for objid, body in objs.items():
        offsets[objid] = len(out)
        out += b"%d 0 obj\n" % objid + body + b"\nendobj\n"
    startxref = len(out)
    if xref:
        size = max(objs) + 1 if objs else 1
        out += b"xref\n0 %d\n" % size
        out += b"0000000000 65535 f \n"
        for i in range(1, size):
            if i in offsets:
                out += b"%010d 00000 n \n" % offsets[i]
            else:
                out += b"0000000000 00000 f \n"
    out += b"trailer\n" + trailer + b"\nstartxref\n%d\n%%%%EOF\n" % startxref + extra_tail
    return bytes(out)


def build_xref_stream(objs, entries, xref_dict, *, xref_objid):
    """A PDF whose xref is a stream: `entries` maps objid -> (type, f2, f3) with W [1 4 2];
    `None` positions are filled in with the object's offset."""
    out = bytearray(b"%PDF-1.5\n")
    offsets = {}
    for objid, body in objs.items():
        offsets[objid] = len(out)
        out += b"%d 0 obj\n" % objid + body + b"\nendobj\n"
    data = bytearray()
    for objid in sorted(entries):
        t, f2, f3 = entries[objid]
        if f2 is None:
            f2 = offsets[objid]
        data += bytes([t]) + f2.to_bytes(4, "big") + f3.to_bytes(2, "big")
    startxref = len(out)
    out += b"%d 0 obj\n<< /Type /XRef /W [1 4 2] /Length %d " % (xref_objid, len(data)) + xref_dict
    out += b" >>\nstream\n" + bytes(data) + b"\nendstream\nendobj\n"
    out += b"startxref\n%d\n%%%%EOF\n" % startxref
    return bytes(out)


def objstm(objects):
    """The body of an object stream holding `{objid: body}`."""
    header = b""
    body = b""
    for objid, b in objects.items():
        header += b"%d %d " % (objid, len(body))
        body += b + b" "
    data = header + body
    return b"<< /Type /ObjStm /N %d /First %d /Length %d >>\nstream\n" % (len(objects), len(header), len(data)) + data + b"\nendstream"


PAGE = b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>"
CATALOG = b"<< /Type /Catalog /Pages 2 0 R >>"


def cases():
    c = {}
    c["length_ref"] = build(
        {
            1: CATALOG,
            2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            3: b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Rotate -90 >>",
            4: b"<< /Length 5 0 R >>\nstream\nBT /F1 12 Tf (Hi) Tj ET\nendstream",
            5: b"23",
        },
        b"<< /Size 6 /Root 1 0 R /Info << /Title (t) >> >>",
    )
    c["length_self"] = build(
        {
            1: CATALOG,
            2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            3: b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>",
            4: b"<< /Length 4 0 R >>\nstream\nBT ET\nendstream",
        },
        b"<< /Size 5 /Root 1 0 R >>",
    )
    c["prev_cycle"] = build(
        {1: CATALOG, 2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>", 3: PAGE},
        b"<< /Size 4 /Root 1 0 R /Prev 999999 >>",
    )
    # /Prev pointing at the xref itself.
    d = c["prev_cycle"]
    pos = d.rindex(b"xref\n0 ")
    c["prev_cycle"] = d.replace(b"/Prev 999999", b"/Prev %06d" % pos)
    c["prev_negative"] = build(
        {1: CATALOG, 2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>", 3: PAGE},
        b"<< /Size 4 /Root 1 0 R /Prev -5 >>",
    )
    c["no_root"] = build({1: CATALOG}, b"<< /Size 2 >>")
    c["page_tree"] = build(
        {
            1: CATALOG,
            2: b"<< /Type /Pages /Kids [3 0 R 6 0 R 2 0 R 7 0 R 3 0 R 8 0 R] /Count 4 /Resources << /Font << >> >> /Rotate 90 >>",
            3: b"<< /Type /Page /Parent 2 0 R /MediaBox <00000102> /CropBox [1 2 3] >>",
            6: b"<< /Type /Pages /Kids [4 0 R 5 0 R] /MediaBox [0 0 100 (200)] >>",
            4: b"<< /type /Page /Rotate -450 /CropBox [(a) 0 0 0] >>",
            5: b"<< /Type /Page /MediaBox [0 0 1 2 3] /Contents [9 0 R 9 0 R] >>",
            7: b"<< /Type /Page /MediaBox [0.5 1.25 -3 1e3] /LastModified (D:2020) /Annots [1 2] /B 3 >>",
            8: b"<< /Type /Page /MediaBox 99 0 R >>",
            9: b"<< /Length 2 >>\nstream\nq Q\nendstream",
        },
        b"<< /Size 10 /Root 1 0 R >>",
    )
    c["page_tree_bad_box"] = build(
        {
            1: CATALOG,
            2: b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>",
            3: PAGE,
            4: b"<< /Type /Page /MediaBox [0 0 /A 1] >>",
        },
        b"<< /Size 5 /Root 1 0 R >>",
    )
    c["page_tree_direct_kid"] = build(
        {
            1: CATALOG,
            2: b"<< /Type /Pages /Kids [3 0 R << /Type /Page >>] /Count 2 >>",
            3: PAGE,
        },
        b"<< /Size 4 /Root 1 0 R >>",
    )
    c["no_pages"] = build(
        {1: b"<< /Type /Catalog >>", 2: PAGE, 3: b"<< /Type /Page /MediaBox [0 0 10 10] >>", 4: b"(x)"},
        b"<< /Size 6 /Root 1 0 R >>",
    )
    c["page_labels"] = build(
        {
            1: b"<< /Type /Catalog /Pages 2 0 R /PageLabels 10 0 R >>",
            2: b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>",
            3: PAGE,
            4: PAGE,
            5: PAGE,
            10: b"<< /Kids [11 0 R 12 0 R] >>",
            11: b"<< /Nums [2 << /S /D /St 7 /P (A-) >> 0 << /S /r >>] >>",
            12: b"<< /Nums [4 << /S /A /St 25 >> 6 << /S /a >> 7 << /S /R /St 3999 >> 8 << /S /X >> 9 << >> 10 << /S /D /P <FEFF00410042> >> 11 << /S /r /St 0 >>] >>",
        },
        b"<< /Size 13 /Root 1 0 R >>",
    )
    c["page_labels_no_zero"] = build(
        {
            1: b"<< /Type /Catalog /Pages 2 0 R /PageLabels << /Nums [3 << /S /D >> 1 << /S /a >>] >> >>",
            2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            3: PAGE,
        },
        b"<< /Size 4 /Root 1 0 R >>",
    )
    c["page_labels_cycle"] = build(
        {
            1: b"<< /Type /Catalog /Pages 2 0 R /PageLabels 10 0 R >>",
            2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            3: PAGE,
            10: b"<< /Kids [10 0 R] >>",
        },
        b"<< /Size 11 /Root 1 0 R >>",
    )
    c["outlines"] = build(
        {
            1: b"<< /Type /Catalog /Pages 2 0 R /Outlines 10 0 R /Names << /Dests 20 0 R >> /Dests << /foo [3 0 R /Fit] >> >>",
            2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            3: PAGE,
            10: b"<< /First 11 0 R /Last 12 0 R >>",
            11: b"<< /Title (One) /Dest [3 0 R /XYZ 0 0 0] /Next 12 0 R /First 13 0 R /Last 13 0 R >>",
            12: b"<< /Title <FEFF0054> /A << /S /GoTo /D (foo) >> /SE 3 0 R >>",
            13: b"<< /Title (Child) /Dest (Chapter1) >>",
            20: b"<< /Kids [21 0 R 22 0 R] >>",
            21: b"<< /Limits [(A) (C)] /Names [(Bar) 1 (Chapter1) [3 0 R /Fit] (Chapter1) 2] >>",
            22: b"<< /Limits [(D) (Z)] /Names [(foo) 5] >>",
        },
        b"<< /Size 23 /Root 1 0 R >>",
    )
    c["outlines_cycle"] = build(
        {
            1: b"<< /Type /Catalog /Pages 2 0 R /Outlines 10 0 R >>",
            2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            3: PAGE,
            10: b"<< /First 11 0 R /Last 12 0 R >>",
            11: b"<< /Title (A) /Dest [3 0 R] /Next 12 0 R >>",
            12: b"<< /Title (B) /Dest [3 0 R] /Next 11 0 R >>",
        },
        b"<< /Size 13 /Root 1 0 R >>",
    )
    stm = objstm({1: CATALOG, 2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>", 4: b"(in stream)"})
    c["xref_stream"] = build_xref_stream(
        {3: PAGE, 5: stm},
        {0: (0, 0, 0), 1: (2, 5, 0), 2: (2, 5, 1), 3: (1, None, 0), 4: (2, 5, 2), 5: (1, None, 0), 6: (2, 5, 9), 7: (2, 8, 0)},
        b"/Size 9 /Root 1 0 R",
        xref_objid=8,
    )
    c["xref_stream_index"] = build_xref_stream(
        {3: PAGE, 5: stm},
        {0: (0, 0, 0), 1: (2, 5, 0), 2: (2, 5, 1), 3: (1, None, 0), 4: (2, 5, 2), 5: (1, None, 0)},
        b"/Size 6 /Index [0 3 3 3] /Root 1 0 R",
        xref_objid=8,
    )
    c["xref_stream_float_index"] = build_xref_stream(
        {3: PAGE, 5: stm},
        {0: (0, 0, 0), 1: (2, 5, 0), 2: (2, 5, 1), 3: (1, None, 0), 4: (2, 5, 2), 5: (1, None, 0)},
        b"/Size 6 /Index [0.0 6] /Root 1 0 R",
        xref_objid=8,
    )
    c["objstm_cycle"] = build_xref_stream(
        {3: PAGE},
        {0: (0, 0, 0), 1: (2, 6, 0), 2: (2, 6, 1), 3: (1, None, 0), 6: (2, 7, 0), 7: (2, 6, 0)},
        b"/Size 8 /Root 3 0 R",
        xref_objid=9,
    )
    enc = b"<< /Filter /Standard /V %s /R %s /P -4 /O <%s> /U <%s> %s >>"
    o = b"00" * 32
    for name, v, r, extra, trailer_extra in [
        ("enc_v3", b"3", b"3", b"", b"/ID [<01> <01>]"),
        ("enc_filter", b"2", b"3", b"", b"/ID [<01> <01>]"),
        ("enc_empty_id", b"2", b"3", b"/Length 128", b"/ID []"),
        ("enc_int_id", b"2", b"3", b"/Length 128", b"/ID [1 2]"),
        ("enc_no_id", b"1", b"2", b"", b""),
        ("enc_len0", b"2", b"3", b"/Length 0", b"/ID [<01> <01>]"),
        ("enc_rev9", b"2", b"9", b"", b"/ID [<01> <01>]"),
        ("enc_v4_bad_cf", b"4", b"4", b"/StmF /StdCF /StrF /StdCF /CF << /StdCF << /CFM /None >> >>", b"/ID [<01> <01>]"),
        ("enc_v4_mismatch", b"4", b"4", b"/StmF /StdCF /StrF /Other", b"/ID [<01> <01>]"),
        ("enc_v4_undefined", b"4", b"4", b"/StmF /StdCF /StrF /StdCF", b"/ID [<01> <01>]"),
        ("enc_v5", b"5", b"6", b"/StmF /StdCF /StrF /StdCF /CF << /StdCF << /CFM /AESV3 >> >> /OE <00> /UE <00>", b"/ID [<01> <01>]"),
    ]:
        e = enc % (v, r, o, o, extra)
        if name == "enc_filter":
            e = e.replace(b"/Standard", b"/Foo")
        c[name] = build(
            {1: CATALOG, 2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>", 3: PAGE, 4: e},
            b"<< /Size 5 /Root 1 0 R /Encrypt 4 0 R " + trailer_extra + b" >>",
        )
    c["xref_weird_lines"] = build(
        {1: CATALOG, 2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>", 3: PAGE},
        b"<< /Size 4 /Root 1 0 R >>",
    ).replace(b"0000000000 65535 f \n", b"0000000000 65535 f \n", 1)
    d = c["xref_weird_lines"]
    i = d.index(b"xref\n0 4\n")
    c["xref_bad_count"] = d[:i] + b"xref\n0 x4\n" + d[i + len(b"xref\n0 4\n"):]
    c["xref_neg_start"] = d[:i] + b"xref\n-1 5\n0000000000 65535 f \n" + d[i + len(b"xref\n0 4\n"):]
    c["trailer_assert"] = d.replace(b"trailer\n", b"trailerX\n")
    return c


def write_all(directory: pathlib.Path):
    directory.mkdir(parents=True, exist_ok=True)
    for old in directory.glob("*.pdf"):
        old.unlink()
    for name, data in cases().items():
        (directory / (name + ".pdf")).write_bytes(data)
