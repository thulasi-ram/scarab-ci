#!/bin/sh
# The canonical clone entrypoint (ADR-0045): SHA-pinned git clone into the
# workspace, credential-free .git/config (token via GIT_ASKPASS from tmpfs),
# opt-in submodules/LFS, and the config-scrub guard before handing the tree
# to the CAS snapshot.
#
# Env contract (set by the engine from the run's trigger context):
#   SCARAB_CLONE_URL         credential-free https clone URL      (required)
#   SCARAB_CLONE_SHA         the pinned commit                    (required)
#   SCARAB_CLONE_DEPTH       "1" (default) | "full"
#   SCARAB_CLONE_SUBMODULES  "true" to fetch submodules recursively
#   SCARAB_CLONE_LFS         "true" to pull LFS objects
#   SCARAB_CLONE_TOKEN_FILE  tmpfs token path (default /scarab/secrets/clone-token;
#                            absent/empty = anonymous public clone)
#   SCARAB_CLONE_USERNAME    basic-auth username (default x-access-token)
#   SCARAB_WORKSPACE         target dir (default /workspace)
set -eu

: "${SCARAB_CLONE_URL:?SCARAB_CLONE_URL is required (a credential-free https URL)}"
: "${SCARAB_CLONE_SHA:?SCARAB_CLONE_SHA is required (the run's pinned commit)}"
dest="${SCARAB_WORKSPACE:-/workspace}"
depth="${SCARAB_CLONE_DEPTH:-1}"

export GIT_ASKPASS=/usr/local/bin/scarab-askpass
export GIT_TERMINAL_PROMPT=0

# The workspace volume is group-owned (Pod fsGroup), not uid-owned — inside
# this single-purpose container git's dubious-ownership guard is noise, and it
# must also cover submodule paths.
git config --global --add safe.directory '*'

mkdir -p "$dest"
cd "$dest"
git init -q .
# The persisted remote URL is credential-free BY CONSTRUCTION (ADR-0045):
# auth happens only through the askpass helper, never the URL or argv.
git remote add origin "$SCARAB_CLONE_URL"

fetch_failed=0
if [ "$depth" = "full" ]; then
  # Complete history and all refs (the forensic / git-describe case), plus the
  # pinned SHA explicitly in case it is a detached head (e.g. a PR merge ref).
  git fetch -q origin '+refs/heads/*:refs/remotes/origin/*' '+refs/tags/*:refs/tags/*' || fetch_failed=1
  git fetch -q origin "$SCARAB_CLONE_SHA" 2>/dev/null || true
else
  git fetch -q --depth 1 origin "$SCARAB_CLONE_SHA" || fetch_failed=1
fi

if [ "$fetch_failed" -ne 0 ] || ! git cat-file -e "$SCARAB_CLONE_SHA^{commit}" 2>/dev/null; then
  # A vanished pinned SHA is TERMINAL (ADR-0045): it will not come back, and
  # it signals an upstream integrity anomaly (history rewritten / ref
  # deleted), not routine churn. Fail fast and loud — never a retry loop.
  echo "SourceUnavailable: the pinned commit $SCARAB_CLONE_SHA no longer exists on the forge" >&2
  echo "  (history rewritten or ref deleted upstream — this run cannot rebuild its source)" >&2
  exit 86
fi

git checkout -q "$SCARAB_CLONE_SHA"

if [ "${SCARAB_CLONE_SUBMODULES:-false}" = "true" ]; then
  git submodule update --init --recursive -q
fi
if [ "${SCARAB_CLONE_LFS:-false}" = "true" ]; then
  git lfs install --local >/dev/null
  git lfs pull -q
fi

# Pre-snapshot guard: refuse to expose a workspace whose .git carries any
# credential (the snapshot includes .git — ADR-0045).
scarab-config-scrub "$dest"

echo "scarab-clone: $SCARAB_CLONE_SHA ready at $dest (depth=$depth)"
