#!/usr/bin/env python3
"""Read CHANGELOG.md for a release (cb31124).

    python3 -I scripts/changelog.py section <version>     # print the [<version>] section's body
    python3 -I scripts/changelog.py notes <tag>           # the section plus the install lines
    python3 -I scripts/changelog.py links [<version>...]  # check the Features entries link docs pages

`section` prints what release.yml uses as the GitHub Release body: everything
between `## [<version>]` and the next `## ` heading, without the heading. It
exits 1 when the section is missing or empty. A leading `v` is dropped, so the
tag name works too. `notes` is the GitHub Release text release.yml writes: the
section, then how to install that version.

`links` checks every bullet under `### Features` in the Unreleased section and in
each named version's section: a bullet must link a docs page that exists. A
docs page is a docs-site URL (`https://thulasi-ram.github.io/scarab-ci/<path>/`,
or the same path from `/scarab-ci/`) that maps to a file under
ui/scarab-docs-ui/src/content/docs, an ADR under docs/adr, or CONTEXT.md; or a
`README.md#<anchor>` link. An `#anchor` must name a heading on that page. It
exits 1 when any bullet fails. Standard library only.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS_SITE = "https://thulasi-ram.github.io/scarab-ci/"
CONTENT = Path("ui/scarab-docs-ui/src/content/docs")

INSTALL = """
## Install

```sh
helm install scarab oci://ghcr.io/thulasi-ram/charts/scarab --version {v} -n scarab --create-namespace
```

Images (linux/amd64 + linux/arm64), all tagged `{v}`:
`ghcr.io/thulasi-ram/scarab-server`, `scarab-clone`, `scarab-wsfetch`, `scarab-results-sidecar`.
"""

# The heading whose entries must link a docs page.
FEATURES = "Features"

LINK = re.compile(r"\]\(([^)\s]+)\)")


def sections(text):
    """{name: body} for every `## [name]` section, in file order."""
    out = {}
    current = None
    for line in text.splitlines():
        m = re.match(r"^## \[([^\]]+)\]", line)
        if m:
            current = m.group(1)
            out[current] = []
        elif line.startswith("## "):
            current = None
        elif current is not None and not re.match(r"^\[[^\]]+\]: ", line):
            # Link definitions at the end of the file belong to no section.
            out[current].append(line)
    return {k: "\n".join(v).strip("\n") for k, v in out.items()}


def subsection(body, name):
    """The text under `### <name>` up to the next `### ` heading."""
    m = re.search(rf"(?ms)^### {re.escape(name)}\s*$(.*?)(?=^### |\Z)", body)
    return m.group(1) if m else ""


def bullets(text):
    """Top-level `- ` bullets, each with its continuation lines joined."""
    items, cur = [], None
    for line in text.splitlines():
        if line.startswith("- "):
            cur = [line[2:]]
            items.append(cur)
        elif cur is not None and line.startswith("  ") and line.strip():
            cur.append(line.strip())
        elif not line.strip():
            cur = None
    return [" ".join(i) for i in items]


def slug(heading):
    """GitHub/Starlight heading anchor: lower-case, punctuation dropped, spaces to dashes."""
    s = heading.strip().lower()
    s = re.sub(r"[^\w\- ]", "", s)
    return s.replace(" ", "-")


def anchors(path):
    text = path.read_text(encoding="utf-8")
    text = re.sub(r"(?ms)^```.*?^```", "", text)  # headings inside code blocks are not headings
    return {slug(m.group(1)) for m in re.finditer(r"(?m)^#{1,6}\s+(.+?)\s*#*\s*$", text)}


def docs_page(root, link):
    """The file a docs link points at, or None. Returns (path, anchor)."""
    target, _, anchor = link.partition("#")
    if target in ("README.md", "./README.md"):
        return root / "README.md", anchor
    for prefix in (DOCS_SITE, "/scarab-ci/"):
        if target.startswith(prefix):
            rel = target[len(prefix):].strip("/")
            break
    else:
        return None, anchor
    if rel == "tech/context":
        return root / "CONTEXT.md", anchor
    if rel.startswith("tech/adr/"):
        p = root / "docs/adr" / (rel[len("tech/adr/"):] + ".md")
        return (p if p.is_file() else None), anchor
    base = root / CONTENT / rel if rel else root / CONTENT / "index"
    for cand in (base.with_suffix(".mdx"), base.with_suffix(".md"), base / "index.mdx", base / "index.md"):
        if cand.is_file():
            return cand, anchor
    return None, anchor


def check_bullet(root, bullet):
    """None when the bullet links an existing docs page, else the reason."""
    links = LINK.findall(bullet)
    if not links:
        return "links no docs page"
    reasons = []
    for link in links:
        page, anchor = docs_page(root, link)
        if page is None:
            reasons.append(f"{link}: not a docs page in this tree")
        elif anchor and anchor not in anchors(page):
            reasons.append(f"{link}: no heading #{anchor} in {page.relative_to(root)}")
        else:
            return None
    return "; ".join(reasons)


def main(argv, root=ROOT):
    if len(argv) < 2 or argv[1] not in ("section", "notes", "links"):
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    text = (root / "CHANGELOG.md").read_text(encoding="utf-8")
    secs = sections(text)
    if argv[1] in ("section", "notes"):
        if len(argv) != 3:
            print(f"usage: changelog.py {argv[1]} <version>", file=sys.stderr)
            return 2
        version = argv[2].removeprefix("v")
        body = secs.get(version, "").strip()
        if not body:
            print(f"error: CHANGELOG.md has no ## [{version}] section, or it is empty", file=sys.stderr)
            return 1
        print(body)
        if argv[1] == "notes":
            print(INSTALL.format(v=version), end="")
        return 0
    names = ["Unreleased"] + [v.removeprefix("v") for v in argv[2:]]
    checked = failed = 0
    for name in dict.fromkeys(names):
        for b in bullets(subsection(secs.get(name, ""), FEATURES)):
            checked += 1
            why = check_bullet(root, b)
            if why:
                failed += 1
                print(f"[{name}] {FEATURES} entry {why}: {b[:80]}", file=sys.stderr)
    print(f"{checked - failed} of {checked} {FEATURES} entries link a docs page")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
