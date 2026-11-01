#!/usr/bin/env bash
# Seed the in-cluster Forgejo (deploy/kind/forgejo.yaml) for the Forgejo cases
# of `just e2e` (git-bug 3863d5e). deploy/kind/up.sh runs it in every mode,
# after Forgejo is rolled out:
#   - an admin user + an API access token
#   - enough repos to force `list_accessible_repos` onto a second page
#   - the working repos with a `.scarab/ci.yaml` on `main`
#   - an org repo reached by a team member and a collaborator (ADR-0069 s7)
#   - with FORGEJO_TUNNEL_URL set (e2e mode): the private repos of the
#     forge_token cases, one with a submodule at the tunnel URL
#
# Writes deploy/kind/.state/forgejo.env (mode 600): the SCARAB_TEST_FORGEJO_*
# contract the tests read, minus the two host-side values the e2e recipe adds
# (its own port-forward URL and the probed host address hooks reach the
# capture listener at).
#
# Idempotent. Every cluster call goes through lib.sh's kc.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"
# shellcheck source=deploy/kind/test-env.sh
. "$here/test-env.sh"   # forward, stop_forwards
state="$here/.state"
trap 'stop_forwards' EXIT

need curl; need python3; need git

owner=scarab-admin
admin_email=scarab-admin@verify.invalid
# Random per stack, kept in .state: in e2e mode the instance is public.
password="$(cat "$state/forgejo-admin-password")"
webhook_secret="$(cat "$state/forgejo-webhook-secret")"
# MUST exceed the adapter's page size (50), so `list_accessible_repos` is forced
# onto a second page: an unpaginated implementation passes a one-page test and
# then truncates a real pick-list.
pad_repos=55
hook_repo="verify-hook"
onboard_repo="verify-onboard"
# ADR-0076 live case: a Pipeline that invokes an installed library, and one
# that reads event.branch (a branch named with `;` must refuse only that one).
library_repo="verify-library"
private_repo="verify-private"
sub_repo="verify-sub"
tunnel_url="${FORGEJO_TUNNEL_URL:-}"
server_base="http://scarab-forgejo.$ns.svc.cluster.local:3000"

forward scarab-forgejo 3000
base="http://127.0.0.1:$fwd_port"

echo "==> waiting for the Forgejo API"
# Any answer below 500 means Forgejo is serving: with sign-in required to view,
# an anonymous /api/v1/version gets 401 or 403.
forgejo_up() {
  local code
  code="$(curl -s -o /dev/null -w '%{http_code}' "$base/api/v1/version" 2>/dev/null || true)"
  [ -n "$code" ] && [ "$code" != 000 ] && [ "$code" -lt 500 ]
}
for _ in $(seq 1 90); do
  if forgejo_up; then break; fi
  sleep 1
done
forgejo_up || { kc logs -n "$ns" deploy/scarab-forgejo --tail 50 >&2 || true; die "Forgejo did not come up"; }

# The `forgejo` CLI inside the container, as the `git` user (it refuses root).
fj() {
  kc exec -n "$ns" deploy/scarab-forgejo -c forgejo -- \
    su-exec git forgejo --config /data/gitea/conf/app.ini "$@"
}

echo "==> ensuring admin user '$owner'"
if ! fj admin user create --admin --username "$owner" --password "$password" \
      --email "$admin_email" --must-change-password=false >/dev/null 2>&1; then
  # It exists. Set the password anyway: .state may have been recreated.
  fj admin user change-password --username "$owner" --password "$password" \
    --must-change-password=false >/dev/null
fi

# The token endpoints take basic auth only. The credentials go to curl on
# stdin (a config file), not argv.
basic() { # curl args...: as the admin user, with basic auth
  printf 'user = "%s:%s"\n' "$owner" "$password" | curl -s -K - "$@"
}
echo "==> revoking earlier scarab-kind-* tokens"
# One is minted per run; without this they pile up, each with every scope.
for _ in $(seq 1 20); do   # a page at a time, until none is left
  old_tokens="$(basic "$base/api/v1/users/$owner/tokens?limit=50" | python3 -I -c '
import json, sys
try:
    rows = json.load(sys.stdin)
except ValueError:
    rows = []
for t in rows if isinstance(rows, list) else []:
    if t.get("name", "").startswith("scarab-kind-"):
        print(t["name"])
')"
  [ -n "$old_tokens" ] || break
  for t in $old_tokens; do
    basic -o /dev/null -X DELETE "$base/api/v1/users/$owner/tokens/$t"
  done
done

echo "==> minting an API access token"
# Token names must be unique per user, so name each run's token for the run.
token="$(fj admin user generate-access-token --username "$owner" \
           --token-name "scarab-kind-$(date +%s)-$$" --scopes all --raw | tr -d '[:space:]')"
[ -n "$token" ] || die "could not mint a Forgejo access token"

if [ -n "$tunnel_url" ]; then
  echo "==> waiting for Forgejo through the tunnel ($tunnel_url)"
  for _ in $(seq 1 90); do
    if curl -sf -H "authorization: token $token" "$tunnel_url/api/v1/version" >/dev/null 2>&1; then break; fi
    sleep 2
  done
  curl -sf -H "authorization: token $token" "$tunnel_url/api/v1/version" >/dev/null \
    || die "Forgejo is not reachable at $tunnel_url"
fi

api() { curl -s -H "authorization: token $token" -H "content-type: application/json" "$@"; }

# Create a repo unless it already exists. $2=auto_init, $3=private (default false).
ensure_repo() {
  local name="$1" auto_init="$2" private="${3:-false}" code
  code="$(api -o /dev/null -w '%{http_code}' "$base/api/v1/repos/$owner/$name")"
  [ "$code" = "200" ] && return 0
  api -o /dev/null -X POST \
    -d "{\"name\":\"$name\",\"auto_init\":$auto_init,\"default_branch\":\"main\",\"private\":$private}" \
    "$base/api/v1/user/repos"
}
# Create the file on main, or update it when its content differs (so a seed
# edited here reaches an existing cluster).
put_file() {
  local repo="$1" path="$2" content="$3" b64 sha
  b64="$(printf '%s' "$content" | python3 -I -c 'import base64,sys;sys.stdout.write(base64.b64encode(sys.stdin.buffer.read()).decode())')"
  # The current file's sha when it exists, "same" when it already holds this
  # content, empty when there is none.
  sha="$(api "$base/api/v1/repos/$owner/$repo/contents/$path?ref=main" | V_WANT="$b64" python3 -I -c '
import base64, json, os, sys
try:
    f = json.load(sys.stdin)
except ValueError:
    sys.exit(0)
if not isinstance(f, dict) or "sha" not in f:
    sys.exit(0)
have = base64.b64decode(f.get("content") or "")
print("same" if have == base64.b64decode(os.environ["V_WANT"]) else f["sha"])
')"
  case "$sha" in
    same) ;;
    '') api -o /dev/null -X POST -d "{\"content\":\"$b64\",\"message\":\"seed $path\",\"branch\":\"main\"}" \
          "$base/api/v1/repos/$owner/$repo/contents/$path" ;;
    *) echo "    updating $repo/$path"
       api -o /dev/null -X PUT -d "{\"content\":\"$b64\",\"sha\":\"$sha\",\"message\":\"seed $path\",\"branch\":\"main\"}" \
          "$base/api/v1/repos/$owner/$repo/contents/$path" ;;
  esac
}

echo "==> seeding the working repos"
ensure_repo "$hook_repo" true
ensure_repo "$onboard_repo" true
# The pipeline the onboarding case's push must trigger. One step, no clone:
# the assertion is that a Run is CREATED from a real delivery.
ci_yaml=$(cat <<'YAML'
on:
  push: {}
steps:
  - id: verify
    image: busybox:latest
    security:
      run_as_root: true
    command: ["sh", "-c", "echo forgejo verification run"]
YAML
)
for r in "$hook_repo" "$onboard_repo"; do
  put_file "$r" ".scarab/ci.yaml" "$ci_yaml"
done
ensure_repo "$library_repo" true
put_file "$library_repo" ".scarab/ci.yaml" "$(cat <<'YAML'
on:
  push: {}
steps:
  - id: say
    invoke: lib:thulasi-ram/notify
    with: { channel: verify }
YAML
)"
put_file "$library_repo" ".scarab/branch.yaml" "$(cat <<'YAML'
on:
  push: {}
steps:
  - id: show
    image: busybox:latest
    security:
      run_as_root: true
    env:
      - [BRANCH, "${{ event.branch }}"]
    command: ["sh", "-c", "echo \"branch=$BRANCH\""]
YAML
)"

echo "==> seeding $pad_repos filler repos (forces a second /user/repos page)"
for i in $(seq -f '%03g' 1 "$pad_repos"); do
  ensure_repo "verify-pad-$i" false
done
total=$((pad_repos + 3))

# ADR-0069 s7: an ORG repo reached three ways (the org's owner, a write TEAM
# member, a read COLLABORATOR), so the live tier can check that
# `list_collaborators` assembles everyone with access, keyed by the numeric id
# `/api/v1/user` reports at sign-in.
team_org="verify-org"
team_repo="team-repo"
team_member="tess"
collaborator="colin"
echo "==> seeding $team_org/$team_repo (owner + team member + collaborator)"
for u in "$team_member" "$collaborator"; do
  fj admin user create --username "$u" --password "$password" \
    --email "$u@verify.invalid" --must-change-password=false >/dev/null 2>&1 || true
done
api -o /dev/null -X POST -d "{\"username\":\"$team_org\"}" "$base/api/v1/orgs"
api -o /dev/null -X POST -d "{\"name\":\"$team_repo\",\"auto_init\":false,\"private\":true}" \
  "$base/api/v1/orgs/$team_org/repos"
# The admin owns the org, so /user/repos lists this repo too.
total=$((total + 1))
team_id="$(api "$base/api/v1/orgs/$team_org/teams" | python3 -I -c '
import json,sys
print(next((t["id"] for t in json.load(sys.stdin) if t["name"]=="devs"), ""))')"
if [ -z "$team_id" ]; then
  team_id="$(api -X POST -d '{"name":"devs","permission":"write","units":["repo.code"],"includes_all_repositories":false}' \
    "$base/api/v1/orgs/$team_org/teams" | python3 -I -c 'import json,sys;print(json.load(sys.stdin)["id"])')"
fi
api -o /dev/null -X PUT "$base/api/v1/teams/$team_id/members/$team_member"
api -o /dev/null -X PUT "$base/api/v1/teams/$team_id/repos/$team_org/$team_repo"
api -o /dev/null -X PUT -d '{"permission":"read"}' \
  "$base/api/v1/repos/$team_org/$team_repo/collaborators/$collaborator"

# The private repos exist in every mode, so the repo count the pagination case
# expects does not depend on the mode.
ensure_repo "$private_repo" true true
ensure_repo "$sub_repo" true true
total=$((total + 2))
if [ -n "$tunnel_url" ]; then
  # The forge_token cases (crates/scarab-e2e/tests/forgejo_tunnel.rs). `ci`
  # clones the private repo through the first-party clone library and reads a
  # file; `own` asks for forge_token in its own Step (refused on a connection
  # with a long-lived token); `sub` clones with submodules, and its one
  # submodule is ANOTHER private repo (no credential).
  echo "==> seeding $private_repo and $sub_repo for the tunnel cases"
  put_file "$private_repo" "marker.txt" "scarab private clone ok"
  put_file "$private_repo" ".scarab/ci.yaml" "$(cat <<'YAML'
on:
  push: {}
steps:
  - id: checkout
    invoke: lib:thulasi-ram/clone
  - id: show
    image: busybox:latest
    security:
      run_as_root: true
    needs: [checkout]
    command: ["sh", "-c", "cat /workspace/marker.txt"]
YAML
)"
  put_file "$private_repo" ".scarab/own.yaml" "$(cat <<'YAML'
on:
  push: {}
steps:
  - id: own
    image: busybox:latest
    security:
      run_as_root: true
    forge_token: read
    command: ["sh", "-c", "true"]
YAML
)"
  put_file "$private_repo" ".scarab/sub.yaml" "$(cat <<'YAML'
on:
  push: {}
steps:
  - id: checkout
    invoke: lib:thulasi-ram/clone
    with: { submodules: true }
YAML
)"
  # The contents API cannot write a gitlink, so commit the submodule with git.
  # Its URL is the tunnel's (a new one each run): the same host as the clone
  # URL, another repo.
  sub_sha="$(api "$base/api/v1/repos/$owner/$sub_repo/branches/main" \
    | python3 -I -c 'import json,sys;print(json.load(sys.stdin)["commit"]["id"])')"
  gitmodules="$(printf '[submodule "sub"]\n\tpath = sub\n\turl = %s/%s/%s.git\n' "$tunnel_url" "$owner" "$sub_repo")"
  work="$(mktemp -d "$state/forgejo-git.XXXXXX")"
  auth=(-c "http.extraHeader=Authorization: token $token")
  git "${auth[@]}" clone -q "$base/$owner/$private_repo.git" "$work/repo"
  if [ "$(cat "$work/repo/.gitmodules" 2>/dev/null || true)" != "$gitmodules" ]; then
    printf '%s\n' "$gitmodules" >"$work/repo/.gitmodules"
    git -C "$work/repo" update-index --add --cacheinfo "160000,$sub_sha,sub"
    git -C "$work/repo" add .gitmodules
    git -C "$work/repo" -c user.name=scarab-verify -c user.email=verify@verify.invalid \
      commit -qm "seed: a private submodule"
    git -C "$work/repo" "${auth[@]}" push -q origin HEAD:main
  fi
  rm -rf "$work"
fi

out="$state/forgejo.env"
echo "==> writing $out"
( umask 077; cat > "$out" <<EOF
# GENERATED by deploy/kind/forgejo.sh. Holds a live API token; never commit.
SCARAB_TEST_FORGEJO=1
# The connection's base URL, as the server Pod reaches it.
SCARAB_TEST_FORGEJO_SERVER_URL=$server_base
SCARAB_TEST_FORGEJO_TOKEN=$token
SCARAB_TEST_FORGEJO_OWNER=$owner
SCARAB_TEST_FORGEJO_HOOK_REPO=$hook_repo
SCARAB_TEST_FORGEJO_ONBOARD_REPO=$onboard_repo
SCARAB_TEST_FORGEJO_LIBRARY_REPO=$library_repo
SCARAB_TEST_FORGEJO_REPO_TOTAL=$total
SCARAB_TEST_FORGEJO_TEAM_ORG=$team_org
SCARAB_TEST_FORGEJO_TEAM_REPO=$team_repo
SCARAB_TEST_FORGEJO_TEAM_MEMBER=$team_member
SCARAB_TEST_FORGEJO_COLLABORATOR=$collaborator
SCARAB_TEST_FORGEJO_WEBHOOK_SECRET=$webhook_secret
# The shared-contract run (crates/scarab-forge-forgejo/tests/contract_live.rs)
# reads owner/name from one variable.
SCARAB_TEST_FORGEJO_REPO=$owner/$hook_repo
SCARAB_TEST_FORGEJO_PRIVATE_REPO=$private_repo
SCARAB_TEST_FORGEJO_ADMIN_PASSWORD=$password
EOF
)
if [ -n "$tunnel_url" ]; then
  cat >> "$out" <<EOF
# The https URL a connection uses for the forge_token cases.
SCARAB_TEST_FORGEJO_HTTPS_URL=$tunnel_url
EOF
fi
echo "==> Forgejo seeded (admin: $owner, $total repos)"
