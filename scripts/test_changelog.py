#!/usr/bin/env python3
"""Tests for scripts/changelog.py: python3 -I scripts/test_changelog.py"""

import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import changelog  # noqa: E402

CHANGELOG = """# Changelog

## [Unreleased]

### Breaking changes

- **Upgrade with `Recreate`.** No docs link needed here.

### Features

- **Libraries.** See [libraries](https://thulasi-ram.github.io/scarab-ci/usage/libraries/#yank-a-version).
- **Policies**, in
  [ADR-0075](/scarab-ci/tech/adr/0075-policy-engine/).
- **Status.** [README](README.md#status).

## [0.2.2] - 2026-10-28

### Bug Fixes

- A fix.

### Misc

- A CI guard.

## [0.2.1] - 2026-10-27

[0.2.2]: https://example.invalid
"""


class ChangelogTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        docs = self.root / changelog.CONTENT / "usage"
        docs.mkdir(parents=True)
        (docs / "libraries.mdx").write_text("---\ntitle: x\n---\n\n## Yank a version\n")
        (self.root / "docs/adr").mkdir(parents=True)
        (self.root / "docs/adr/0075-policy-engine.md").write_text("# 0075. Policy engine\n")
        (self.root / "README.md").write_text("# Scarab\n\n## Status\n")
        (self.root / "CHANGELOG.md").write_text(CHANGELOG)

    def tearDown(self):
        self.tmp.cleanup()

    def run_main(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = changelog.main(["changelog.py", *args], root=self.root)
        return code, out.getvalue(), err.getvalue()

    def test_section_is_the_body_up_to_the_next_heading(self):
        code, out, _ = self.run_main("section", "v0.2.2")
        self.assertEqual(code, 0)
        self.assertEqual(out, "### Bug Fixes\n\n- A fix.\n\n### Misc\n\n- A CI guard.\n")

    def test_notes_add_the_install_lines_for_the_version(self):
        code, out, _ = self.run_main("notes", "v0.2.2")
        self.assertEqual(code, 0)
        self.assertTrue(out.startswith("### Bug Fixes\n\n- A fix.\n\n### Misc\n\n- A CI guard.\n\n## Install\n"))
        self.assertIn("--version 0.2.2 ", out)
        self.assertEqual(self.run_main("notes", "v0.9.0")[0], 1)

    def test_a_missing_or_empty_section_fails(self):
        self.assertEqual(self.run_main("section", "0.9.0")[0], 1)
        self.assertEqual(self.run_main("section", "0.2.1")[0], 1)

    def test_features_entries_with_docs_links_pass(self):
        code, out, err = self.run_main("links")
        self.assertEqual((code, err), (0, ""))
        self.assertIn("3 of 3", out)

    def test_a_features_entry_without_a_docs_page_fails(self):
        text = CHANGELOG.replace("README.md#status", "README.md#nope").replace(
            "usage/libraries/", "usage/nothing/"
        )
        (self.root / "CHANGELOG.md").write_text(text + "\n")
        code, _, err = self.run_main("links")
        self.assertEqual(code, 1)
        self.assertIn("usage/nothing", err)
        self.assertIn("no heading #nope", err)

    def test_only_features_entries_need_a_docs_page(self):
        # The Breaking changes entry links nothing and is not checked; nor is
        # a bullet under an old `### New` heading.
        (self.root / "CHANGELOG.md").write_text(CHANGELOG.replace("### Features", "### New"))
        code, out, err = self.run_main("links")
        self.assertEqual((code, err), (0, ""))
        self.assertIn("0 of 0 Features", out)


if __name__ == "__main__":
    unittest.main()
