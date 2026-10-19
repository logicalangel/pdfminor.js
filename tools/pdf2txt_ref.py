"""The upstream tools/pdf2txt.py with tools/pyref.py applied (the reference for difftest)."""

import runpy
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pyref  # noqa: E402

pyref.apply()
UPSTREAM_TOOL = Path(__file__).resolve().parent.parent / "upstream" / "pdfminer.six" / "tools" / "pdf2txt.py"
sys.argv[0] = str(UPSTREAM_TOOL)
runpy.run_path(str(UPSTREAM_TOOL), run_name="__main__")
