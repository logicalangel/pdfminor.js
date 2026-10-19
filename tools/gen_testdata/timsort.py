"""Differential test data for crates/pdfminer/src/pyrt/timsort.rs (CPython's list.sort).

    uv run python tools/gen_testdata/timsort.py

The keys are not stored: each case is a spec from which both this script and the Rust test
build the same float keys (with ties and NaNs) using splitmix64. The expected result is
`sorted(range(n), key=keys.__getitem__)`, stored in full for short lists and as an FNV-1a hash
otherwise. With NaN keys `<` is not a total order, so only the exact CPython algorithm (same
comparisons in the same order) gives the same permutation.
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import rng, write  # noqa: E402

MASK = (1 << 64) - 1


class SplitMix64:
    def __init__(self, seed: int) -> None:
        self.state = seed & MASK

    def next(self) -> int:
        self.state = (self.state + 0x9E3779B97F4A7C15) & MASK
        z = self.state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK
        return z ^ (z >> 31)


def make_keys(seed: int, n: int, pattern: int, distinct: int, nan_pm: int, noise_pm: int) -> list[float]:
    """Must match `make_keys` in crates/pdfminer/tests/timsort.rs."""
    g = SplitMix64(seed)
    keys = []
    period = distinct + 1
    for i in range(n):
        r = g.next()
        if pattern == 0:  # random
            v = r % distinct
        elif pattern == 1:  # ascending
            v = i * distinct // n
        elif pattern == 2:  # descending
            v = (n - i) * distinct // n
        elif pattern == 3:  # sawtooth
            v = i % period
        else:  # descending sawtooth
            v = period - i % period
        if pattern != 0 and g.next() % 1000 < noise_pm:
            v = r % distinct
        if g.next() % 1000 < nan_pm:
            keys.append(float("nan"))
        else:
            keys.append(float(v))
    return keys


def fnv1a(perm: list[int]) -> int:
    h = 0xCBF29CE484222325
    for i in perm:
        for b in i.to_bytes(4, "little"):
            h ^= b
            h = (h * 0x100000001B3) & MASK
    return h


def main() -> None:
    r = rng("timsort")
    cases = []
    sizes = list(range(0, 80)) + [100, 127, 128, 129, 255, 256, 257, 500, 1000, 1023, 1024, 2048, 3000, 5000, 8191]
    for n in sizes:
        for _ in range(6 if n <= 300 else 4):
            spec = {
                "seed": r.randrange(1 << 64),
                "n": n,
                "pattern": r.randrange(5),
                "distinct": r.choice([1, 2, 3, 10, 100, 10**6]),
                "nan_pm": r.choice([0, 0, 5, 50, 300, 900, 1000]),
                "noise_pm": r.choice([0, 10, 100]),
            }
            keys = make_keys(spec["seed"], n, spec["pattern"], spec["distinct"], spec["nan_pm"], spec["noise_pm"])
            perm = sorted(range(n), key=keys.__getitem__)
            case = {**spec, "seed": str(spec["seed"]), "fnv": str(fnv1a(perm))}
            if n <= 100:
                case["perm"] = perm
            cases.append(case)
    write("timsort", "listsort", cases)


if __name__ == "__main__":
    main()
