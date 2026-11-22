#!/usr/bin/env bash
# Record the Forgejo compare answers the changed-paths tests of
# crates/scarab-forge-forgejo read (ADR-0081): `just test record-compare`.
#
# Against the in-cluster Forgejo of the kind stack (deploy/kind/forgejo.yaml),
# seeded by deploy/kind/forgejo.sh (`just stack up` runs it). The script:
#   - deletes and recreates the repo `<admin>/record-compare`, and deletes it
#     again when the script exits, done or not: the Forgejo tier counts the
#     repos the admin can list (SCARAB_TEST_FORGEJO_REPO_TOTAL), and a repo
#     left behind makes that count one too many;
#   - builds three pushes with git in a temporary directory and pushes every
#     branch:
#       ff_merge    a fast-forward from B to M = merge(c1, s1): c1 renames
#                   docs/old.md to docs/new.md, drops tmp/scratch.txt and edits
#                   conf/app.yaml; s1 (the second parent) adds
#                   side/feature.txt and edits conf/app.yaml; the merge
#                   resolves conf/app.yaml and adds conf/resolution.yaml.
#       forced      before = P (a commit on B), after = x2 (x1, x2 on B): P is
#                   not an ancestor of after.
#       pull_merge  before = P2 (a commit on B); c1 on P2; X on B; after =
#                   merge(X, c1), first parent X: a `git pull` merge pushed
#                   back. P2 is an ancestor of after but not on its
#                   first-parent line.
#   - asks `GET /repos/{o}/{r}/compare/{before}...{after}?verification=false`
#     for each, as the adapter does, and writes the answers to
#     tests/fixtures/compare_<name>.json, and the ids with the Forgejo version
#     to tests/fixtures/compare_ids.json.
#
# The tests find commits by their message and read before/after from
# compare_ids.json, so a recording only changes ids and timestamps. Review the
# diff before committing it. No Scarab server is involved and nothing else in
# the cluster is changed.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"
# shellcheck source=deploy/kind/test-env.sh
. "$here/test-env.sh"   # forward, stop_forwards
state="$here/.state"

need curl; need python3; need git

[ -f "$state/forgejo.env" ] || die "no $state/forgejo.env: run \`just stack up\` first"
# shellcheck disable=SC1091
. "$state/forgejo.env"
token="${SCARAB_TEST_FORGEJO_TOKEN:?forgejo.env has no SCARAB_TEST_FORGEJO_TOKEN}"
owner="${SCARAB_TEST_FORGEJO_OWNER:?forgejo.env has no SCARAB_TEST_FORGEJO_OWNER}"
repo=record-compare
out="$repo_root/crates/scarab-forge-forgejo/tests/fixtures"

work="$(mktemp -d "$state/record-compare.XXXXXX")"
base=""
api() { curl -sf -H "authorization: token $token" -H "content-type: application/json" "$@"; }
cleanup() {
  if [ -n "$base" ]; then
    api -o /dev/null -X DELETE "$base/api/v1/repos/$owner/$repo" \
      || echo "warning: could not delete $owner/$repo; delete it before \`just test e2e\`" >&2
  fi
  stop_forwards
  rm -rf "$work"
}
trap cleanup EXIT

forward scarab-forgejo 3000
base="http://127.0.0.1:$fwd_port"

version="$(api "$base/api/v1/version" | python3 -I -c 'import json,sys;print(json.load(sys.stdin)["version"])')"
echo "==> Forgejo $version at $base"

echo "==> recreating $owner/$repo"
api -o /dev/null -X DELETE "$base/api/v1/repos/$owner/$repo" || true
api -o /dev/null -X POST \
  -d "{\"name\":\"$repo\",\"auto_init\":false,\"default_branch\":\"main\",\"private\":false}" \
  "$base/api/v1/user/repos"

g() {
  git -C "$work" -c user.name=scarab-record -c user.email=record@verify.invalid \
    -c commit.gpgsign=false -c init.defaultBranch=main "$@"
}
put() { mkdir -p "$work/$(dirname "$1")"; printf '%s\n' "$2" >"$work/$1"; }
commit() { g add -A; g commit -qm "$1"; g rev-parse HEAD; }

g init -q
put conf/app.yaml "replicas: 1"
put docs/old.md "old docs"
put tmp/scratch.txt "scratch"
B="$(commit "base")"

echo "==> ff_merge"
g checkout -q -b ff-side "$B"
put conf/app.yaml "replicas: 2"
put side/feature.txt "feature"
commit "side: add feature" >/dev/null
g checkout -q -b ff "$B"
put conf/app.yaml "replicas: 3"
g mv docs/old.md docs/new.md
g rm -q tmp/scratch.txt
commit "line: rename docs, drop scratch" >/dev/null
g merge -q --no-ff --no-commit ff-side >/dev/null 2>&1 || true   # conflicts on conf/app.yaml
put conf/app.yaml "replicas: 4"
put conf/resolution.yaml "resolved in the merge"
ff_after="$(commit "merge: side into ff")"

echo "==> forced"
g checkout -q -b forced-before "$B"
put src/dropped.rs "// dropped by the forced push"
forced_before="$(commit "forced: dropped")"
g checkout -q -b forced "$B"
put src/one.rs "// one"
commit "forced: first" >/dev/null
put src/two.rs "// two"
forced_after="$(commit "forced: second")"

echo "==> pull_merge"
g checkout -q -b pull-before "$B"
put feature/start.txt "start"
pull_before="$(commit "feature: start")"
g checkout -q -b pull-feature
put feature/change.txt "change"
commit "feature: change" >/dev/null
g checkout -q -b pull-other "$B"
put other/change.txt "change"
commit "other: change" >/dev/null
g merge -q --no-ff -m "merge: feature into other line" pull-feature
pull_after="$(g rev-parse HEAD)"

echo "==> pushing"
g -c "http.extraHeader=Authorization: token $token" push -q "$base/$owner/$repo.git" --all

record() { # $1 name, $2 before, $3 after
  api "$base/api/v1/repos/$owner/$repo/compare/$2...$3?verification=false" \
    | python3 -I -c 'import json,sys;json.dump(json.load(sys.stdin),sys.stdout,indent=2);print()' \
    >"$out/compare_$1.json"
  echo "    wrote $out/compare_$1.json"
}
record ff_merge "$B" "$ff_after"
record forced "$forced_before" "$forced_after"
record pull_merge "$pull_before" "$pull_after"

python3 -I - "$out/compare_ids.json" "$version" "$(date -u +%Y-%m-%d)" \
  "$B" "$ff_after" "$forced_before" "$forced_after" "$pull_before" "$pull_after" <<'PY'
import json, sys
path, version, day, b, ffa, fb, fa, pb, pa = sys.argv[1:]
ids = {
    "recorded_from": f"Forgejo {version}, deploy/kind/record-compare.sh, {day}",
    "ff_merge": {"before": b, "after": ffa},
    "forced": {"before": fb, "after": fa},
    "pull_merge": {"before": pb, "after": pa},
}
with open(path, "w") as f:
    json.dump(ids, f, indent=2)
    f.write("\n")
PY
echo "    wrote $out/compare_ids.json"
echo "==> done: review \`git diff $out\` and commit the recording ($owner/$repo is deleted on exit)"
