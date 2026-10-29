#!/bin/sh
# The canonical clone entrypoint (ADR-0045): SHA-pinned git clone into the
# workspace, credential-free .git/config (token via GIT_ASKPASS from tmpfs),
# opt-in submodules/LFS, and the config-scrub guard before handing the tree
# to the CAS snapshot.
#
# Credential scope: the token is offered only for SCARAB_CLONE_URL's own
# repository over https (see scarab-askpass). Submodules are fetched with no
# credential, so a private submodule fails with SubmoduleUnavailable. LFS
# objects come from the cloned repository's own LFS endpoint; an `lfs.url` in
# the repository's `.lfsconfig` is overridden.
#
# Env contract (set by the engine from the run's trigger context):
#   SCARAB_CLONE_URL         credential-free https clone URL      (required;
#                            a non-https URL is cloned without the token)
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
# Put the repository path in git's credential prompts, so scarab-askpass can
# check it (without this git names only the host, and the helper refuses).
# git-lfs reads the same setting.
git config --global credential.useHttpPath true

case "$SCARAB_CLONE_URL" in
  https://*) ;;
  *)
    if [ -s "${SCARAB_CLONE_TOKEN_FILE:-/scarab/secrets/clone-token}" ]; then
      echo "scarab-clone: $SCARAB_CLONE_URL is not https; cloning without the token" >&2
    fi
    ;;
esac

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
  # Submodule URLs come from the checked-out .gitmodules, which the commit
  # being built controls (a fork pull request included). They are fetched
  # with no askpass helper, so no credential: an empty GIT_ASKPASS turns git's
  # helper lookup off, and GIT_TERMINAL_PROMPT=0 stops it asking anywhere else.
  if ! GIT_ASKPASS= git submodule update --init --recursive -q; then
    # DOCUMENTED LIMITATION (ADR-0045): the clone credential covers only the
    # run's repository, so private submodules are not supported.
    echo "SubmoduleUnavailable: recursive submodule checkout failed for $SCARAB_CLONE_URL" >&2
    echo "  Submodules are fetched without the run's clone credential, so a private" >&2
    echo "  submodule cannot be fetched. Make the submodule public or vendor it." >&2
    exit 87
  fi
fi
if [ "${SCARAB_CLONE_LFS:-false}" = "true" ]; then
  git lfs install --local >/dev/null
  # Pin the LFS endpoint to the cloned repository's own. `.lfsconfig` is
  # content of the commit being built, and git-lfs answers an endpoint on the
  # clone URL's host with the git remote's credential, so an `lfs.url`
  # naming another repository there would receive the token. Command-line
  # config overrides `.lfsconfig`.
  lfs_repo=$SCARAB_CLONE_URL
  while [ "${lfs_repo%/}" != "$lfs_repo" ]; do lfs_repo=${lfs_repo%/}; done
  lfs_url="${lfs_repo%.git}.git/info/lfs"
  # NB: `git lfs pull` has no -q flag; quiet it via stdout redirect.
  if ! git -c lfs.url="$lfs_url" lfs pull >/dev/null; then
    echo "LfsUnavailable: git-lfs pull failed for $SCARAB_CLONE_URL" >&2
    echo "  (LFS objects unreachable or not covered by the clone credential)" >&2
    exit 88
  fi
fi

# Pre-snapshot guard: refuse to expose a workspace whose .git carries any
# credential (the snapshot includes .git — ADR-0045).
scarab-config-scrub "$dest"

echo "scarab-clone: $SCARAB_CLONE_SHA ready at $dest (depth=$depth)"
