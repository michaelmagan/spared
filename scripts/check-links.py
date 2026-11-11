#!/usr/bin/env python3
"""Check that every relative markdown link in the public docs resolves to a file.

Usage: python3 scripts/check-links.py [ROOT]   (ROOT defaults to the repository root)
Checks README.md, AGENTS.md, CONTRIBUTING.md, CHANGELOG.md, docs/*.md and
examples/README.md. Skips fenced code blocks, URLs and mailto links. A "#fragment" is
checked against the target file's headings when the target is a markdown file.
Also checks that every `bench/...` and `scripts/...` path named in those files and in the
READMEs under apps/ and bench/ exists (paths under bench/work, which holds generated files,
and paths with glob or placeholder characters are skipped).
"""
import re
import sys
from pathlib import Path

root = Path(sys.argv[1] if len(sys.argv) > 1 else Path(__file__).resolve().parent.parent).resolve()
files = [root / n for n in ("README.md", "AGENTS.md", "CONTRIBUTING.md", "CHANGELOG.md", "examples/README.md")]
files += sorted((root / "docs").glob("*.md"))
files = [f for f in files if f.exists()]

INLINE = re.compile(r"(?<!\!)\[[^\]]*\]\(\s*<?([^)\s>]+)>?(?:\s+\"[^\"]*\")?\s*\)|!\[[^\]]*\]\(\s*<?([^)\s>]+)>?")
REFDEF = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*(\S+)")


def slug(heading):
    s = re.sub(r"[`*_]", "", heading.strip().lower())
    s = re.sub(r"[^\w\- ]", "", s)
    return s.replace(" ", "-")


def anchors(path):
    out = set()
    fence = False
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if line.lstrip().startswith("```"):
            fence = not fence
        elif not fence and line.startswith("#"):
            out.add(slug(line.lstrip("#")))
    return out


bad = []
checked = 0
for f in files:
    fence = False
    for n, line in enumerate(f.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
        if line.lstrip().startswith("```"):
            fence = not fence
            continue
        if fence:
            continue
        targets = [a or b for a, b in INLINE.findall(line)]
        m = REFDEF.match(line)
        if m:
            targets.append(m.group(1))
        for t in targets:
            if re.match(r"^[a-z][a-z0-9+.-]*:", t, re.I) or t.startswith("//"):
                continue
            path, _, frag = t.partition("#")
            checked += 1
            dest = (f.parent / path) if path else f
            if path.startswith("/"):
                dest = root / path.lstrip("/")
            if not dest.exists():
                bad.append(f"{f.relative_to(root)}:{n}: missing {t}")
            elif frag and dest.suffix == ".md" and slug(frag) not in anchors(dest):
                bad.append(f"{f.relative_to(root)}:{n}: no heading for #{frag} in {dest.relative_to(root)}")

PATH_RE = re.compile(r"(?<![\w./~$-])((?:bench|scripts)/[A-Za-z0-9_./-]+)([*{<$\[]?)")
path_files = list(files) + sorted(root.glob("apps/*/README.md")) + sorted(root.glob("bench/**/*.md"))
paths_checked = 0
for f in dict.fromkeys(path_files):
    for n, line in enumerate(f.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
        for m in PATH_RE.finditer(line):
            ref, follow = m.group(1).rstrip(".,;:-"), m.group(2)
            if follow or ref.startswith("bench/work") or ref.endswith(("-", "/")) and not (root / ref).is_dir():
                continue
            paths_checked += 1
            if not (root / ref).exists():
                bad.append(f"{f.relative_to(root)}:{n}: missing path {ref}")

print(f"{len(files)} files, {checked} relative links and {paths_checked} bench/scripts paths checked")
for b in bad:
    print(b)
sys.exit(1 if bad else 0)
