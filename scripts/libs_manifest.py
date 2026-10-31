#!/usr/bin/env python3
"""Write the first-party library manifest for a release (ADR-0076 §5).

    libs_manifest.py --libs libs --repository github.com/<owner>/<repo> \
        --commit <sha> --release v1.2.3 [--previous prev.json] > libs-manifest.json

Each `<libs>/*.yaml` is a library named after its file, published by the
repository's owner. Its source is `<repository>//libs/<file>@<commit>`, which
Scarab fetches at that commit. The manifest keeps every released version of
each library, oldest first: the `--previous` release's manifest is carried
over, and a library whose content changed gets a new version. A version whose
hash is listed in `<libs>/security-fixes.txt` is marked `security_fix`.

Standard library only, so the release job needs no install step.
"""

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

SCHEMA = 1
SEGMENT = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9._-]{0,99}$")
COMMIT = re.compile(r"^([0-9a-f]{40}|[0-9a-f]{64})$")
HASH = re.compile(r"^[0-9a-f]{64}$")


class ManifestError(Exception):
    pass


def security_fixes(libs: Path) -> set:
    path = libs / "security-fixes.txt"
    if not path.exists():
        return set()
    out = set()
    for n, line in enumerate(path.read_text().splitlines(), 1):
        entry = line.split("#", 1)[0].strip()
        if not entry:
            continue
        if not HASH.match(entry):
            raise ManifestError(f"{path}:{n}: `{entry}` is not a SHA-256 hash")
        out.add(entry)
    return out


def build(libs: Path, repository: str, commit: str, release: str, previous=None) -> dict:
    parts = repository.split("/")
    if len(parts) != 3 or not all(SEGMENT.match(p) for p in parts[1:]):
        raise ManifestError(f"--repository `{repository}` is not <host>/<owner>/<repo>")
    if not COMMIT.match(commit):
        raise ManifestError(f"--commit `{commit}` is not a full commit id")
    if not re.match(r"^v\d+\.\d+\.\d+", release):
        raise ManifestError(f"--release `{release}` is not a v<major>.<minor>.<patch> tag")
    publisher = parts[1]
    fixes = security_fixes(libs)

    libraries = {}
    for lib in (previous or {}).get("libraries", []):
        libraries[(lib["publisher"], lib["name"])] = {
            "publisher": lib["publisher"],
            "name": lib["name"],
            "versions": list(lib["versions"]),
        }

    files = sorted(libs.glob("*.yaml")) if libs.is_dir() else []
    for f in files:
        name = f.stem
        if not SEGMENT.match(name):
            raise ManifestError(f"{f}: `{name}` is not a library name")
        content = f.read_bytes()
        if not re.search(rb"(?m)^kind:\s*library\s*$", content):
            raise ManifestError(f"{f}: a library file starts with `kind: library`")
        digest = hashlib.sha256(content).hexdigest()
        entry = libraries.setdefault(
            (publisher, name), {"publisher": publisher, "name": name, "versions": []}
        )
        versions = entry["versions"]
        if versions and versions[-1]["content_hash"] == digest:
            continue
        versions.append(
            {
                "content_hash": digest,
                "source": f"{repository}//libs/{f.name}@{commit}",
                "release": release,
                "security_fix": digest in fixes,
            }
        )

    return {
        "schema": SCHEMA,
        "release": release,
        "libraries": [libraries[k] for k in sorted(libraries)],
    }


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--libs", type=Path, default=Path("libs"))
    p.add_argument("--repository", required=True)
    p.add_argument("--commit", required=True)
    p.add_argument("--release", required=True)
    p.add_argument("--previous", type=Path)
    args = p.parse_args(argv)
    previous = None
    if args.previous and args.previous.exists():
        previous = json.loads(args.previous.read_text())
        if previous.get("schema") != SCHEMA:
            print(f"error: {args.previous} has schema {previous.get('schema')}", file=sys.stderr)
            return 1
    try:
        manifest = build(args.libs, args.repository, args.commit, args.release, previous)
    except ManifestError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    json.dump(manifest, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
