"""Makes the reference pdfminer.six deterministic, so the port can be compared with it.

pdfminer.six uses `id(obj)` (a memory address) in two places where it reaches the output:

- `LTLayoutContainer.group_textboxes` breaks ties between equal distances with `id(box)`, so
  the same PDF can group text differently from one Python run to the next
  (docs/DIVERGENCES.md, `heap-id-ties`).
- `PDFPageInterpreter.do_EI` names inline images `str(id(obj))` (`inline-image-id`).

The port uses creation order for the first and a counter for the second. `apply()` makes the
Python reference do the same; every reference run (tools/oracle.py, tools/pdf2txt_ref.py, the
test data generators) calls it first.
"""

import itertools
from typing import Any


def apply() -> None:
    import pdfminer.layout as layout
    import pdfminer.pdfinterp as pdfinterp

    serial = itertools.count()
    orig_component_init = layout.LTComponent.__init__

    def component_init(self: Any, bbox: Any) -> None:
        self._serial = next(serial)
        orig_component_init(self, bbox)

    layout.LTComponent.__init__ = component_init  # type: ignore[method-assign]
    layout.id = lambda o: o._serial  # type: ignore[attr-defined]

    # Inline images: the n-th inline image of the run (counting from 1) is named "inline<n>".
    inline = itertools.count(1)
    pdfinterp.id = lambda o: f"inline{next(inline)}"  # type: ignore[attr-defined]
