"""Tracks which upstream pdfminer.six version each Rust file was ported from.

UPSTREAM.toml maps every upstream file that matters (modules, tools, tests) to its Rust
counterpart and records the git blob of the upstream file the Rust file currently matches. After
the submodule moves to a new release, this tool lists exactly what changed and shows the diffs to
port.

    uv run python tools/upstream.py status          # what changed upstream since each file's sync
    uv run python tools/upstream.py diff [PATH...]  # upstream diffs to port (all changed files by default)
    uv run python tools/upstream.py mark PATH...    # record PATH (a python path) as synced to the submodule
    uv run python tools/upstream.py mark --all      # same, for every file (after a full sync)

See docs/UPDATING.md for the whole procedure.
"""

import argparse
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UPSTREAM = ROOT / "upstream" / "pdfminer.six"
MANIFEST = ROOT / "UPSTREAM.toml"


def git(*args: str) -> str:
    return subprocess.run(
        ["git", "-C", str(UPSTREAM), *args], check=True, capture_output=True, text=True
    ).stdout.strip()


def blob(path: str, rev: str = "HEAD") -> str | None:
    try:
        return git("rev-parse", f"{rev}:{path}")
    except subprocess.CalledProcessError:
        return None


def load() -> dict:
    return tomllib.loads(MANIFEST.read_text())


def save(data: dict) -> None:
    lines = [
        "# The pdfminer.six release this port follows, and for every upstream file the git blob",
        "# its Rust counterpart was last synced with. Maintained by tools/upstream.py; see",
        "# docs/UPDATING.md.",
        "",
        "[upstream]",
        f'version = "{data["upstream"]["version"]}"',
        f'commit = "{data["upstream"]["commit"]}"',
        "",
    ]
    for f in data["file"]:
        lines.append("[[file]]")
        lines.append(f'python = "{f["python"]}"')
        lines.append(f'rust = "{f["rust"]}"')
        lines.append(f'how = "{f["how"]}"')
        lines.append(f'synced_blob = "{f["synced_blob"]}"')
        if f.get("note"):
            lines.append(f'note = "{f["note"]}"')
        lines.append("")
    MANIFEST.write_text("\n".join(lines))


def tracked_upstream_files() -> list[str]:
    files = git("ls-files", "pdfminer", "tools", "tests").splitlines()
    return [f for f in files if f.endswith(".py") and not f.endswith("__init__.py")]


def cmd_status(_: argparse.Namespace) -> int:
    data = load()
    head = git("rev-parse", "HEAD")
    described = git("describe", "--tags", "--always")
    print(f"submodule: {described} ({head[:10]})")
    print(f"manifest:  {data['upstream']['version']} ({data['upstream']['commit'][:10]})")
    known = {f["python"] for f in data["file"]}
    changed = 0
    for f in data["file"]:
        now = blob(f["python"])
        if now is None:
            print(f"  REMOVED   {f['python']}  -> remove or rework {f['rust']}")
            changed += 1
        elif now != f["synced_blob"]:
            print(f"  CHANGED   {f['python']}  -> port into {f['rust']} ({f['how']})")
            changed += 1
    for p in tracked_upstream_files():
        if p not in known:
            print(f"  NEW       {p}  -> decide where it goes and add it to UPSTREAM.toml")
            changed += 1
    if changed == 0:
        print("everything is in sync")
    return 1 if changed else 0


def cmd_diff(args: argparse.Namespace) -> int:
    data = load()
    wanted = set(args.paths)
    for f in data["file"]:
        if wanted and f["python"] not in wanted:
            continue
        now = blob(f["python"])
        if now == f["synced_blob"]:
            continue
        print(f"===== {f['python']}  ->  {f['rust']} ({f['how']})")
        if now is None:
            print("(removed upstream)")
            continue
        out = subprocess.run(
            ["git", "-C", str(UPSTREAM), "diff", "--no-color", f["synced_blob"], now],
            capture_output=True,
            text=True,
        ).stdout
        print(out)
    return 0


def cmd_mark(args: argparse.Namespace) -> int:
    data = load()
    head = git("rev-parse", "HEAD")
    targets = set(args.paths)
    for f in data["file"]:
        if args.all or f["python"] in targets:
            now = blob(f["python"])
            if now is None:
                print(f"{f['python']} no longer exists upstream", file=sys.stderr)
                return 1
            f["synced_blob"] = now
            targets.discard(f["python"])
    if targets:
        print(f"not in UPSTREAM.toml: {sorted(targets)}", file=sys.stderr)
        return 1
    if args.all:
        data["upstream"]["commit"] = head
        data["upstream"]["version"] = git("describe", "--tags", "--always")
    save(data)
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("status").set_defaults(func=cmd_status)
    d = sub.add_parser("diff")
    d.add_argument("paths", nargs="*")
    d.set_defaults(func=cmd_diff)
    m = sub.add_parser("mark")
    m.add_argument("paths", nargs="*")
    m.add_argument("--all", action="store_true")
    m.set_defaults(func=cmd_mark)
    args = p.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
