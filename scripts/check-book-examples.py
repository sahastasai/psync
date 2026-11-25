#!/usr/bin/env python3
"""Check that the book's example reports are what psync prints today.

For every Rust example in book/src that is directly followed by a `text` block holding a report
(it starts with `region`), run `psync-expand` on the example and compare its explanation with the
block. Exits with status 1 and a diff for each example that drifted.

    scripts/check-book-examples.py          # from the repository root
"""
import difflib
import json
import pathlib
import re
import subprocess
import sys
import tempfile
import textwrap

root = pathlib.Path(__file__).resolve().parent.parent
subprocess.run(
    ["cargo", "build", "-q", "-p", "psync-graph", "--features", "cli", "--bin", "psync-expand"],
    cwd=root,
    check=True,
)
expand = root / "target" / "debug" / "psync-expand"
blocks = re.compile(r"```(\S*)\n(.*?)```", re.S)


def norm(s):
    return [line.rstrip() for line in textwrap.dedent(s).strip().splitlines()]


total = drifted = 0
with tempfile.TemporaryDirectory() as tmp:
    for md in sorted((root / "book" / "src").rglob("*.md")):
        found = [(m.group(1), m.group(2), m.start()) for m in blocks.finditer(md.read_text())]
        for (lang, code, pos), (next_lang, report, _) in zip(found, found[1:]):
            if not lang.startswith("rust") or next_lang != "text":
                continue
            if not report.lstrip().startswith("region") or not re.search(r"\bfn\b", code):
                continue
            total += 1
            where = f"{md.relative_to(root)} (offset {pos})"
            file = pathlib.Path(tmp) / f"{md.stem}-{pos}.rs"
            file.write_text(textwrap.dedent(code))
            # The annotated function if there is one, else the first function.
            lines = code.splitlines()
            marked = [i for i, l in enumerate(lines) if re.match(r"\s*#\[(psync::)?graph", l)]
            start = marked[0] if marked else 0
            line = next(i for i, l in enumerate(lines[start:], start + 1) if re.search(r"\bfn\b", l))
            run = subprocess.run([expand, file, str(line + 1)], capture_output=True, text=True)
            try:
                got = json.loads(run.stdout)["explain"]
            except (ValueError, KeyError):
                drifted += 1
                print(f"!! {where}: psync-expand failed\n{run.stderr[-500:]}")
                continue
            if norm(got) != norm(report):
                drifted += 1
                print(f"== {where}")
                for d in difflib.unified_diff(norm(report), norm(got), "book", "psync", lineterm="", n=0):
                    print("   ", d)
print(f"{total - drifted} of {total} example reports match psync")
sys.exit(1 if drifted else 0)
