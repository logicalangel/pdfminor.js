"""Differential test: runs pdfminer.six (Python) and the port (Rust) on a corpus of PDFs and
reports every difference. An empty report is the definition of "the port is correct".

    uv run python tools/difftest.py                 # whole corpus, every mode
    uv run python tools/difftest.py -k layout       # only modes whose name contains "layout"
    uv run python tools/difftest.py --files a.pdf   # only these files
    uv run python tools/difftest.py --corpus DIR    # add a directory of PDFs (also: $PDFMINER_CORPUS,
                                                    # a list of directories separated by ':')

The corpus is the upstream samples (with the passwords the upstream tests use), the PDFs under
tests/corpus/, and any extra directories. Each file runs in every mode below: the layout dump
(`tools/oracle.py layout` vs `pdfminer-rs dump`) under several LAParams, and `pdf2txt.py` vs
`pdfminer-rs pdf2txt` in every output format. Outputs are compared byte for byte, exit codes too.

Python results are cached under tests/out/oracle/, keyed by the upstream commit, the file's hash
and the mode, so reruns only pay for the Rust side.
"""

import argparse
import hashlib
import json
import os
import re
import resource
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UPSTREAM = ROOT / "upstream" / "pdfminer.six"
SAMPLES = UPSTREAM / "samples"
OUT = ROOT / "tests" / "out"
RUST_BIN = ROOT / "target" / "release" / "pdfminer-rs"
PY = sys.executable

# The passwords the upstream tests use for the encrypted samples.
PASSWORDS = {
    "encryption/aes-128.pdf": ["foo", "", "wrong"],
    "encryption/aes-128-m.pdf": ["foo", ""],
    "encryption/aes-256.pdf": ["foo", ""],
    "encryption/aes-256-m.pdf": ["foo", ""],
    "encryption/aes-256-r6.pdf": ["usersecret", "ownersecret", ""],
    "encryption/rc4-40.pdf": ["foo", ""],
    "encryption/rc4-128.pdf": ["foo", ""],
    "encryption/base.pdf": [""],
    "encryption/encrypted_doc_no_id.pdf": [""],
}

LAYOUT_MODES = {
    "layout-default": "default",
    "layout-none": "none",
    "layout-noflow": json.dumps({"boxes_flow": None}),
    "layout-vertical-all": json.dumps({"detect_vertical": True, "all_texts": True}),
    "layout-tight": json.dumps({"char_margin": 1.0, "line_margin": 0.3, "word_margin": 0.2, "boxes_flow": -0.5}),
}

# Differences that are documented divergences (docs/DIVERGENCES.md): reported, not failures.
KNOWN_DIVERGENCES = {
    # A tag property Python decodes with charset_normalizer's heuristics (BOM + U+FF09).
    ("contrib/issue-598-cmap-other-fonts.pdf", "txt-tag"): "charset-normalizer",
}

# DIVERGE(bigint): a few i64 uses of an int beyond i64 differ (see docs/DIVERGENCES.md). Fuzzed
# files make such ints at random, so instead of listing files, a difference counts as this
# divergence when it goes away once every int of 19+ digits is written as repr(float(n)).
BIG_INT = re.compile(rb"(?<![\w.])-?[0-9]{19,}(?![\w.])")


def _as_float(m: re.Match[bytes]) -> bytes:
    n = int(m.group())
    if abs(n) < 2**63:
        return m.group()
    try:
        return repr(float(n)).encode()
    except OverflowError:
        return m.group()  # inf in the port: behaviour differs too, not only the text


def only_bigint(py: bytes, rs: bytes) -> bool:
    return BIG_INT.sub(_as_float, py) == BIG_INT.sub(_as_float, rs)


PDF2TXT_MODES = {
    "txt-text": ["-t", "text"],
    "txt-text-nolaparams": ["-t", "text", "-n"],
    "txt-text-vertical": ["-t", "text", "-V", "-A"],
    "txt-html": ["-t", "html"],
    "txt-html-exact": ["-t", "html", "-Y", "exact", "-s", "1.5"],
    "txt-html-loose": ["-t", "html", "-Y", "loose"],
    "txt-xml": ["-t", "xml"],
    "txt-xml-strip": ["-t", "xml", "--strip-control", "-n"],
    "txt-hocr": ["-t", "hocr"],
    "txt-tag": ["-t", "tag"],
    "txt-text-rotated": ["-t", "text", "-R", "90"],
}


@dataclass(frozen=True)
class Case:
    pdf: Path
    password: str
    mode: str

    @property
    def name(self) -> str:
        rel = self.pdf.relative_to(SAMPLES) if self.pdf.is_relative_to(SAMPLES) else self.pdf
        pw = f" [password={self.password!r}]" if self.password else ""
        return f"{rel}{pw} :: {self.mode}"


def upstream_commit() -> str:
    return subprocess.run(
        ["git", "-C", str(UPSTREAM), "rev-parse", "HEAD"], capture_output=True, text=True, check=True
    ).stdout.strip()


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def python_cmd(case: Case) -> list[str]:
    if case.mode in LAYOUT_MODES:
        return [PY, str(ROOT / "tools" / "oracle.py"), "layout", str(case.pdf),
                "--laparams", LAYOUT_MODES[case.mode], "--password", case.password]
    return [PY, str(ROOT / "tools" / "pdf2txt_ref.py"), *PDF2TXT_MODES[case.mode],
            "-P", case.password, str(case.pdf)]


def rust_cmd(case: Case) -> list[str]:
    if case.mode in LAYOUT_MODES:
        return [str(RUST_BIN), "dump", str(case.pdf), "--laparams", LAYOUT_MODES[case.mode],
                "--password", case.password]
    return [str(RUST_BIN), "pdf2txt", *PDF2TXT_MODES[case.mode], "-P", case.password, str(case.pdf)]


TIMEOUT = 600
TIMED_OUT = (-999, b"TIMEOUT")


# Address space of each Python reference run (--python-memory; None: no limit). Some fuzzed
# files make pdfminer.six allocate without bound (a /W range up to 10**20); the cap turns that
# into the MemoryError Python raises on a machine without the memory, instead of the OOM killer
# taking the runner. The port raises MemoryError for those ranges too.
PYTHON_MEMORY: int | None = None


def _limit_memory() -> None:
    assert PYTHON_MEMORY is not None
    resource.setrlimit(resource.RLIMIT_AS, (PYTHON_MEMORY, PYTHON_MEMORY))


def run(cmd: list[str], limit_memory: bool = False) -> tuple[int, bytes]:
    try:
        p = subprocess.run(
            cmd,
            capture_output=True,
            timeout=TIMEOUT,
            preexec_fn=_limit_memory if limit_memory and PYTHON_MEMORY else None,
        )
    except subprocess.TimeoutExpired:
        return TIMED_OUT
    return (p.returncode, p.stdout)


def python_result(case: Case, commit: str) -> tuple[int, bytes]:
    key = hashlib.sha256(
        json.dumps([commit, sha256(case.pdf), case.password, python_cmd(case)[2:]]).encode()
    ).hexdigest()
    cache = OUT / "oracle" / key[:2] / key
    if cache.exists():
        data = cache.read_bytes()
        code, _, out = data.partition(b"\n")
        return (int(code), out)
    code, out = run(python_cmd(case), limit_memory=True)
    if (code, out) == TIMED_OUT or code < 0:
        return (code, out)  # not cached: rerun with a longer --timeout, or after a kill
    cache.parent.mkdir(parents=True, exist_ok=True)
    cache.write_bytes(str(code).encode() + b"\n" + out)
    return (code, out)


def first_difference(a: bytes, b: bytes) -> str:
    la, lb = a.split(b"\n"), b.split(b"\n")
    for i, (x, y) in enumerate(zip(la, lb, strict=False)):
        if x != y:
            j = next((k for k, (p, q) in enumerate(zip(x, y, strict=False)) if p != q), min(len(x), len(y)))
            lo = max(0, j - 80)
            return (
                f"line {i + 1}, column {j + 1}:\n"
                f"    python: ...{x[lo:j + 120].decode('utf-8', 'replace')}\n"
                f"    rust:   ...{y[lo:j + 120].decode('utf-8', 'replace')}"
            )
    return f"python has {len(la)} lines, rust has {len(lb)}"


def corpus(extra: list[str], files: list[str]) -> list[Path]:
    if files:
        return [Path(f).resolve() for f in files]
    pdfs = sorted(SAMPLES.rglob("*.pdf"))
    pdfs += sorted((ROOT / "tests" / "corpus").rglob("*.pdf"))
    dirs = list(extra) + [d for d in os.environ.get("PDFMINER_CORPUS", "").split(":") if d]
    for d in dirs:
        pdfs += sorted(Path(d).rglob("*.pdf"))
    return pdfs


def cases(pdfs: list[Path], keyword: str | None) -> list[Case]:
    modes = [m for m in [*LAYOUT_MODES, *PDF2TXT_MODES] if not keyword or keyword in m]
    out = []
    for pdf in pdfs:
        rel = str(pdf.relative_to(SAMPLES)) if pdf.is_relative_to(SAMPLES) else ""
        for pw in PASSWORDS.get(rel, [""]):
            for m in modes:
                out.append(Case(pdf, pw, m))
    return out


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("-k", dest="keyword", help="only modes whose name contains this")
    p.add_argument("--files", nargs="*", default=[])
    p.add_argument("--corpus", action="append", default=[])
    p.add_argument("-j", type=int, default=os.cpu_count() or 4)
    p.add_argument("--no-build", action="store_true", help="do not run cargo build --release first")
    p.add_argument("--max-report", type=int, default=50)
    p.add_argument("--python-memory", type=int, help="address space limit of each Python run, in MB")
    p.add_argument("--timeout", type=int, default=TIMEOUT, help="seconds per run (Python takes 17 minutes on "
                   "nonfree/i1040nr.pdf rotated)")
    args = p.parse_args()
    globals()["TIMEOUT"] = args.timeout
    if args.python_memory:
        globals()["PYTHON_MEMORY"] = args.python_memory << 20

    if not args.no_build:
        subprocess.run(["cargo", "build", "--release", "-q", "-p", "pdfminer-cli"], cwd=ROOT, check=True)

    commit = upstream_commit()
    todo = cases(corpus(args.corpus, args.files), args.keyword)

    bigint: dict[str, str] = {}

    def check(case: Case) -> tuple[Case, str | None]:
        py = python_result(case, commit)
        if py == TIMED_OUT:
            return (case, f"python timed out after {TIMEOUT}s (rerun with a longer --timeout)")
        rs = run(rust_cmd(case))
        if py == rs:
            return (case, None)
        if py[0] == rs[0] and only_bigint(py[1], rs[1]):
            bigint[case.name] = "bigint"
        if py[0] != rs[0]:
            return (case, f"exit code: python {py[0]}, rust {rs[0]}\n" + first_difference(py[1], rs[1]))
        return (case, first_difference(py[1], rs[1]))

    failures = []
    known = []
    with ThreadPoolExecutor(max_workers=args.j) as pool:
        for case, diff in pool.map(check, todo):
            if diff is None:
                continue
            rel = str(case.pdf.relative_to(SAMPLES)) if case.pdf.is_relative_to(SAMPLES) else str(case.pdf)
            divergence = KNOWN_DIVERGENCES.get((rel, case.mode)) or bigint.get(case.name)
            if divergence:
                known.append(case)
                print(f"KNOWN {case.name} (DIVERGE({divergence}))", flush=True)
                continue
            failures.append((case, diff))
            if len(failures) <= args.max_report:
                print(f"DIFF {case.name}\n  {diff}\n", flush=True)
    print(
        f"{len(todo) - len(failures) - len(known)}/{len(todo)} identical, {len(known)} known divergences, "
        f"{len(failures)} different"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
