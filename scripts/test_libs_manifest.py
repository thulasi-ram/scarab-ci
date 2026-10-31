#!/usr/bin/env python3
"""Tests for scripts/libs_manifest.py, over scripts/testdata/libs_manifest.

    python3 -I scripts/test_libs_manifest.py
"""

import hashlib
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import libs_manifest as lm  # noqa: E402

FIXTURE = HERE / "testdata" / "libs_manifest"
REPO = "github.com/thulasi-ram/scarab-ci"
C1 = "1" * 40
C2 = "2" * 40


class Manifest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        shutil.copy(FIXTURE / "clone.yaml", self.dir / "clone.yaml")

    def tearDown(self):
        shutil.rmtree(self.dir)

    def hash(self, name="clone.yaml"):
        return hashlib.sha256((self.dir / name).read_bytes()).hexdigest()

    def test_a_library_is_listed_with_publisher_source_and_hash(self):
        m = lm.build(self.dir, REPO, C1, "v0.3.0")
        self.assertEqual(m["schema"], 1)
        self.assertEqual(m["release"], "v0.3.0")
        [lib] = m["libraries"]
        self.assertEqual((lib["publisher"], lib["name"]), ("thulasi-ram", "clone"))
        self.assertEqual(
            lib["versions"],
            [
                {
                    "content_hash": self.hash(),
                    "source": f"{REPO}//libs/clone.yaml@{C1}",
                    "release": "v0.3.0",
                    "security_fix": False,
                }
            ],
        )

    def test_an_unchanged_library_keeps_its_version_and_a_changed_one_adds_one(self):
        first = lm.build(self.dir, REPO, C1, "v0.3.0")
        same = lm.build(self.dir, REPO, C2, "v0.3.1", first)
        self.assertEqual(same["libraries"], first["libraries"])

        (self.dir / "clone.yaml").write_text(
            (self.dir / "clone.yaml").read_text().replace("0.3.0", "0.3.2")
        )
        (self.dir / "security-fixes.txt").write_text(
            f"# the patched clone image\n{self.hash()}  # CVE-2026-1234\n"
        )
        fixed = lm.build(self.dir, REPO, C2, "v0.3.2", same)
        versions = fixed["libraries"][0]["versions"]
        self.assertEqual([v["release"] for v in versions], ["v0.3.0", "v0.3.2"])
        self.assertEqual([v["security_fix"] for v in versions], [False, True])
        self.assertEqual(versions[1]["source"], f"{REPO}//libs/clone.yaml@{C2}")

    def test_a_removed_library_keeps_its_history(self):
        first = lm.build(self.dir, REPO, C1, "v0.3.0")
        (self.dir / "clone.yaml").unlink()
        later = lm.build(self.dir, REPO, C2, "v0.4.0", first)
        self.assertEqual(later["libraries"], first["libraries"])

    def test_the_fixture_matches_the_manifest_the_server_reads(self):
        # crates/scarab-server reads expected.json in its own test, so the
        # two sides agree on the format.
        import json

        built = lm.build(FIXTURE, REPO, C1, "v0.3.0")
        self.assertEqual(built, json.loads((FIXTURE / "expected.json").read_text()))

    def test_no_library_yet_is_an_empty_manifest(self):
        empty = Path(tempfile.mkdtemp())
        try:
            self.assertEqual(lm.build(empty, REPO, C1, "v0.3.0")["libraries"], [])
            self.assertEqual(lm.build(empty / "missing", REPO, C1, "v0.3.0")["libraries"], [])
        finally:
            shutil.rmtree(empty)

    def test_bad_inputs_are_refused(self):
        with self.assertRaises(lm.ManifestError):
            lm.build(self.dir, REPO, "main", "v0.3.0")
        with self.assertRaises(lm.ManifestError):
            lm.build(self.dir, "thulasi-ram/scarab-ci", C1, "v0.3.0")
        with self.assertRaises(lm.ManifestError):
            lm.build(self.dir, REPO, C1, "0.3.0")
        (self.dir / "pipeline.yaml").write_text("on: { push: {} }\nsteps: []\n")
        with self.assertRaises(lm.ManifestError):
            lm.build(self.dir, REPO, C1, "v0.3.0")
        (self.dir / "pipeline.yaml").unlink()
        (self.dir / "security-fixes.txt").write_text("not-a-hash\n")
        with self.assertRaises(lm.ManifestError):
            lm.build(self.dir, REPO, C1, "v0.3.0")


if __name__ == "__main__":
    unittest.main()
