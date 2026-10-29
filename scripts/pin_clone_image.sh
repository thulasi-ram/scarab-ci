#!/usr/bin/env bash
# Pin libs/clone.yaml to a published scarab-clone build (ADR-0076 §9).
#
#   scripts/pin_clone_image.sh <tag>        # e.g. 0.3.0, sha-<commit>, edge
#
# The first-party clone library pins its image by digest, and a release
# cannot pin the image it builds itself: the digest exists only after the
# build, and the manifest hashes libs/clone.yaml as committed. So a change to
# docker/clone is published by image.yml first (on main as `sha-<commit>`, or
# by a release), then this script writes that build's multi-arch digest into
# libs/clone.yaml, and the release after that ships it. Commit the result, and
# add its hash to libs/security-fixes.txt when the image fixes a security
# problem.
set -euo pipefail
tag="${1:?usage: scripts/pin_clone_image.sh <tag>}"
repo="ghcr.io/thulasi-ram/scarab-clone"
root="$(cd "$(dirname "$0")/.." && pwd)"
lib="$root/libs/clone.yaml"

digest="$(docker buildx imagetools inspect "$repo:$tag" --format '{{json .Manifest}}' \
  | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["digest"])')"
case "$digest" in
  sha256:????????????????????????????????????????????????????????????????) ;;
  *) echo "error: $repo:$tag has no manifest digest (got '$digest')" >&2; exit 1 ;;
esac

python3 -I - "$lib" "$repo:$tag@$digest" <<'PY'
import re, sys
path, image = sys.argv[1], sys.argv[2]
text = open(path).read()
new, n = re.subn(r"(?m)^(\s*image:\s*)ghcr\.io/thulasi-ram/scarab-clone\S*$", r"\g<1>" + image, text)
if n != 1:
    sys.exit(f"error: expected one scarab-clone image line in {path}, found {n}")
open(path, "w").write(new)
PY
echo "pinned $lib to $repo:$tag@$digest"
