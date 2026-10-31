#!/usr/bin/env bash
# Stand up a REAL Forgejo and seed it for the verification tier (git-bug 3863d5e):
#   - the container (compose.yaml), install-locked, webhooks allowed to loopback
#   - an admin user + an API access token
#   - enough repos to force `list_accessible_repos` onto a second page
#   - two working repos with a `.scarab/ci.yaml` on `main`
#
# Writes two gitignored env files the recipe and the tests read:
#   .env.generated  — SCARAB_TEST_FORGEJO_*   (the tests' contract)
#   .env.scarab     — SCARAB_* overlay for deploy/local-proc/up.sh
#
# Idempotent: safe to re-run. Requires docker + curl + python3.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
compose="docker compose -f $here/compose.yaml"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: '$1' not found on PATH" >&2; exit 1; }; }
need docker; need curl; need python3

# Defaults live in the committed template; a local .env overrides them.
set -a
# shellcheck disable=SC1091
. "$here/.env.example"
[ -f "$here/.env" ] && . "$here/.env"
set +a

base="http://127.0.0.1:${FORGEJO_HTTP_PORT}"
owner="$FORGEJO_ADMIN_USER"
hook_repo="verify-hook"
onboard_repo="verify-onboard"
# ADR-0076 live case: a Pipeline that invokes an installed library, and one
# that reads event.branch (a branch named with `;` must refuse only that one).
library_repo="verify-library"

# Tunnel mode (FORGEJO_TUNNEL=1, opt-in): the forge_token cases. A forge token
# is only ever sent to an https clone URL: the server refuses to mint for any
# other (forge_token.rs), and the pinned clone image's askpass refuses to hand
# it over (docker/clone/scarab-askpass). The clone URL comes from the
# connection's base URL, which the host server AND the kind Pods must both
# resolve, over TLS the pinned image trusts. A cloudflared quick tunnel is the
# one local setup that gives all three, so this mode makes network calls; the
# default mode makes none. The instance is then reachable from the internet:
# a random admin password, sign-in required to view, and the tunnel stops
# with down.sh.
tunnel="${FORGEJO_TUNNEL:-0}"
tunnel_url=""
private_repo="verify-private"
sub_repo="verify-sub"
if [ "$tunnel" = 1 ]; then
  need cloudflared; need git; need openssl
  if [ "$FORGEJO_ADMIN_PASSWORD" = "scarab-verify-pw-0" ]; then
    FORGEJO_ADMIN_PASSWORD="$(openssl rand -hex 16)"
  fi
fi

# Same rule as deploy/local-proc/up.sh: never seed, or grade, an instance this
# script did not start. Here the port bind would normally fail loudly inside
# `compose up -d`, but if a FOREIGN Forgejo already holds the port we would
# happily mint tokens and repos in someone else's instance — and the tier would
# report on it. Refuse instead, unless the listener is our own compose service
# (re-running this script is meant to be idempotent). /dev/tcp, not lsof: no new
# tool dependency beyond docker/curl/python3.
if (exec 3<>"/dev/tcp/127.0.0.1/$FORGEJO_HTTP_PORT") 2>/dev/null \
   && [ -z "$($compose ps -q forgejo 2>/dev/null)" ]; then
  {
    echo "error: something is already listening on 127.0.0.1:$FORGEJO_HTTP_PORT and it is not"
    echo "  this compose project's Forgejo (project: scarab-forgejo-verify)."
    echo "  Find it with:  lsof -nP -iTCP:$FORGEJO_HTTP_PORT -sTCP:LISTEN"
    echo "  Then stop it, or set FORGEJO_HTTP_PORT in deploy/local-forgejo/.env."
  } >&2
  exit 1
fi

if [ "$tunnel" = 1 ]; then
  if [ -f "$here/.tunnel.pid" ]; then
    old="$(cat "$here/.tunnel.pid" 2>/dev/null || true)"
    if [ -n "$old" ] && ps -o comm= -p "$old" 2>/dev/null | grep -q cloudflared; then
      kill "$old" 2>/dev/null || true
    fi
  fi
  echo "==> starting a cloudflared quick tunnel to $base"
  nohup cloudflared tunnel --no-autoupdate --url "$base" >"$here/.tunnel.log" 2>&1 &
  echo $! >"$here/.tunnel.pid"
  # Wait for a registered connection before anything resolves the name: a
  # lookup that runs first can be cached as NXDOMAIN.
  for _ in $(seq 1 60); do
    tunnel_url="$(grep -o 'https://[a-z0-9-]*\.trycloudflare\.com' "$here/.tunnel.log" | head -n 1 || true)"
    if [ -n "$tunnel_url" ] && grep -q "Registered tunnel connection" "$here/.tunnel.log"; then break; fi
    sleep 1
  done
  if [ -z "$tunnel_url" ] || ! grep -q "Registered tunnel connection" "$here/.tunnel.log"; then
    echo "error: cloudflared did not register a tunnel; log:" >&2
    tail -n 20 "$here/.tunnel.log" >&2
    exit 1
  fi
  echo "    $tunnel_url"
  export FORGEJO_ROOT_URL="$tunnel_url/"
  export FORGEJO_REQUIRE_SIGNIN_VIEW=true
else
  export FORGEJO_ROOT_URL="$base/"
  export FORGEJO_REQUIRE_SIGNIN_VIEW=false
fi

echo "==> starting Forgejo ($FORGEJO_IMAGE) on $base"
$compose up -d

echo "==> waiting for the Forgejo API"
for _ in $(seq 1 90); do
  if curl -sf "$base/api/v1/version" >/dev/null 2>&1; then break; fi
  sleep 1
done
curl -sf "$base/api/v1/version" >/dev/null || {
  echo "error: Forgejo did not come up; logs:" >&2
  $compose logs --tail 50 forgejo >&2
  exit 1
}
echo "    $(curl -s "$base/api/v1/version")"

# `forgejo` CLI inside the container. The image exports GITEA_CUSTOM/WORK_DIR, but
# some builds do not, so fall back to the on-disk config path.
fj() {
  if ! $compose exec -T -u git forgejo forgejo "$@" 2>/dev/null; then
    $compose exec -T -u git forgejo forgejo --config /data/gitea/conf/app.ini "$@"
  fi
}

echo "==> ensuring admin user '$owner'"
if ! fj admin user create --admin --username "$owner" --password "$FORGEJO_ADMIN_PASSWORD" \
      --email "$FORGEJO_ADMIN_EMAIL" --must-change-password=false >/dev/null 2>&1; then
  echo "    (already exists)"
fi

echo "==> minting an API access token"
# Token names must be unique per user, so name each run's token for the run. The
# instance is throwaway; stale tokens die with the volume.
token_name="scarab-verify-$(date +%s)-$$"
token="$(fj admin user generate-access-token --username "$owner" \
           --token-name "$token_name" --scopes all --raw | tr -d '[:space:]')"
[ -n "$token" ] || { echo "error: could not mint an access token" >&2; exit 1; }

if [ "$tunnel" = 1 ]; then
  # A volume kept from a default-mode run still has the committed password.
  fj admin user change-password --username "$owner" --password "$FORGEJO_ADMIN_PASSWORD" \
    --must-change-password=false >/dev/null
  echo "==> waiting for Forgejo through the tunnel"
  for _ in $(seq 1 90); do
    if curl -sf -H "authorization: token $token" "$tunnel_url/api/v1/version" >/dev/null 2>&1; then break; fi
    sleep 2
  done
  curl -sf -H "authorization: token $token" "$tunnel_url/api/v1/version" >/dev/null || {
    echo "error: Forgejo is not reachable at $tunnel_url" >&2
    tail -n 20 "$here/.tunnel.log" >&2
    exit 1
  }
fi

# Create a repo unless it already exists. $2=auto_init, $3=private (default false).
ensure_repo() {
  local name="$1" auto_init="$2" private="${3:-false}" code
  code="$(curl -s -o /dev/null -w '%{http_code}' -H "authorization: token $token" \
            "$base/api/v1/repos/$owner/$name")"
  [ "$code" = "200" ] && return 0
  curl -sS -o /dev/null -X POST -H "authorization: token $token" \
    -H "content-type: application/json" \
    -d "{\"name\":\"$name\",\"auto_init\":$auto_init,\"default_branch\":\"main\",\"private\":$private}" \
    "$base/api/v1/user/repos"
}

echo "==> seeding the two working repos"
ensure_repo "$hook_repo" true
ensure_repo "$onboard_repo" true

# The pipeline the onboarding leg's push must trigger. One step, no clone — the
# assertion is that a Run is CREATED from a real delivery, and the kind cluster
# cannot reach a Forgejo published on the docker host anyway.
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
put_file() {
  local repo="$1" path="$2" content="$3" code
  code="$(curl -s -o /dev/null -w '%{http_code}' -H "authorization: token $token" \
            "$base/api/v1/repos/$owner/$repo/contents/$path?ref=main")"
  [ "$code" = "200" ] && return 0
  local b64; b64="$(printf '%s' "$content" | python3 -c 'import base64,sys;sys.stdout.write(base64.b64encode(sys.stdin.buffer.read()).decode())')"
  curl -sS -o /dev/null -X POST -H "authorization: token $token" \
    -H "content-type: application/json" \
    -d "{\"content\":\"$b64\",\"message\":\"seed $path\",\"branch\":\"main\"}" \
    "$base/api/v1/repos/$owner/$repo/contents/$path"
}
for r in "$hook_repo" "$onboard_repo"; do
  put_file "$r" ".scarab/ci.yaml" "$ci_yaml"
done
ensure_repo "$library_repo" true
library_ci_yaml=$(cat <<'YAML'
on:
  push: {}
steps:
  - id: say
    invoke: lib:thulasi-ram/notify
    with: { channel: verify }
YAML
)
branch_yaml=$(cat <<'YAML'
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
)
put_file "$library_repo" ".scarab/ci.yaml" "$library_ci_yaml"
put_file "$library_repo" ".scarab/branch.yaml" "$branch_yaml"

echo "==> seeding $FORGEJO_PAD_REPOS filler repos (forces a second /user/repos page)"
for i in $(seq -f '%03g' 1 "$FORGEJO_PAD_REPOS"); do
  ensure_repo "verify-pad-$i" false
done
total=$((FORGEJO_PAD_REPOS + 3))
echo "    $total repos reachable by the token"

# ADR-0069 s7: an ORG repo reached three different ways — the org's owner (the
# Owners team), a write TEAM member, and a read COLLABORATOR — so the live tier
# can check that `list_collaborators` assembles everyone with access, keyed by
# the same numeric id `/api/v1/user` reports at sign-in. A forge listing that
# missed any of them would make a re-sync revoke that person.
team_org="verify-org"
team_repo="team-repo"
team_member="tess"
collaborator="colin"
api() { curl -s -H "authorization: token $token" -H "content-type: application/json" "$@"; }
echo "==> seeding $team_org/$team_repo (owner + team member + collaborator)"
for u in "$team_member" "$collaborator"; do
  fj admin user create --username "$u" --password "$FORGEJO_ADMIN_PASSWORD" \
    --email "$u@verify.invalid" --must-change-password=false >/dev/null 2>&1 || true
done
api -o /dev/null -X POST -d "{\"username\":\"$team_org\"}" "$base/api/v1/orgs"
api -o /dev/null -X POST -d "{\"name\":\"$team_repo\",\"auto_init\":false,\"private\":true}" \
  "$base/api/v1/orgs/$team_org/repos"
# The admin owns the org, so /user/repos lists this repo too (b3678d46 added
# it without counting it, and the pagination case expected one repo too few).
total=$((total + 1))
team_id="$(api "$base/api/v1/orgs/$team_org/teams" | python3 -c '
import json,sys
print(next((t["id"] for t in json.load(sys.stdin) if t["name"]=="devs"), ""))')"
if [ -z "$team_id" ]; then
  team_id="$(api -X POST -d '{"name":"devs","permission":"write","units":["repo.code"],"includes_all_repositories":false}' \
    "$base/api/v1/orgs/$team_org/teams" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')"
fi
api -o /dev/null -X PUT "$base/api/v1/teams/$team_id/members/$team_member"
api -o /dev/null -X PUT "$base/api/v1/teams/$team_id/repos/$team_org/$team_repo"
api -o /dev/null -X PUT -d '{"permission":"read"}' \
  "$base/api/v1/repos/$team_org/$team_repo/collaborators/$collaborator"

clone_hash=""
if [ "$tunnel" = 1 ]; then
  # The forge_token cases (crates/scarab-e2e/tests/forgejo_tunnel.rs). A
  # private repo with three Pipelines: `ci` clones it through the first-party
  # clone library and reads a file; `own` asks for forge_token in its own
  # Step (refused on a connection with a long-lived token); `sub` clones with
  # submodules, and its one submodule is ANOTHER private repo (no credential).
  echo "==> seeding $private_repo and $sub_repo (private)"
  ensure_repo "$private_repo" true true
  ensure_repo "$sub_repo" true true
  total=$((total + 2))
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
  # Its URL is the tunnel's: the same host as the clone URL, another repo.
  sub_sha="$(api "$base/api/v1/repos/$owner/$sub_repo/branches/main" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["commit"]["id"])')"
  gitmodules="$(printf '[submodule "sub"]\n\tpath = sub\n\turl = %s/%s/%s.git\n' "$tunnel_url" "$owner" "$sub_repo")"
  work="$(mktemp -d)"
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

  # The release manifest the server reads, so binding the org installs the
  # first-party clone library from this tree's libs/ (ADR-0076 §5). The
  # server fetches it from github.com at the commit, so it must be pushed.
  root="$(cd "$here/../.." && pwd)"
  libs_commit="$(git -C "$root" log -1 --format=%H -- libs/)"
  if ! git -C "$root" diff --quiet "$libs_commit" -- libs/ \
     || [ -z "$(git -C "$root" branch -r --contains "$libs_commit" 2>/dev/null)" ]; then
    echo "error: libs/ must be committed and pushed: the server fetches libs/clone.yaml" >&2
    echo "  from github.com at commit $libs_commit" >&2
    exit 1
  fi
  python3 "$root/scripts/libs_manifest.py" --libs "$root/libs" \
    --repository github.com/thulasi-ram/scarab-ci --commit "$libs_commit" \
    --release v0.0.0-forgejo-verify >"$here/.libs-manifest.json"
  clone_hash="$(python3 -c 'import json,sys
m=json.load(open(sys.argv[1]))
print(next(l["versions"][-1]["content_hash"] for l in m["libraries"] if l["name"]=="clone"))' "$here/.libs-manifest.json")"
fi

echo "==> writing $here/.env.generated"
cat > "$here/.env.generated" <<EOF
# GENERATED by deploy/local-forgejo/up.sh — do not edit, do not commit.
SCARAB_TEST_FORGEJO=1
SCARAB_TEST_FORGEJO_URL=$base
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
SCARAB_TEST_FORGEJO_CALLBACK_HOST=$FORGEJO_CALLBACK_HOST
SCARAB_TEST_FORGEJO_WEBHOOK_SECRET=$FORGEJO_WEBHOOK_SECRET
# The existing shared-contract run (crates/scarab-forge-forgejo/tests/contract_live.rs)
# reads owner/name from one variable.
SCARAB_TEST_FORGEJO_REPO=$owner/$hook_repo
EOF
if [ "$tunnel" = 1 ]; then
  cat >> "$here/.env.generated" <<EOF
# Tunnel mode: the https URL a connection uses for the forge_token cases.
SCARAB_TEST_FORGEJO_HTTPS_URL=$tunnel_url
SCARAB_TEST_FORGEJO_PRIVATE_REPO=$private_repo
SCARAB_TEST_FORGEJO_ADMIN_PASSWORD=$FORGEJO_ADMIN_PASSWORD
SCARAB_TEST_CLONE_HASH=$clone_hash
EOF
fi

echo "==> writing $here/.env.scarab (overlay for the local-proc server)"
cat > "$here/.env.scarab" <<EOF
# GENERATED by deploy/local-forgejo/up.sh — sourced by deploy/local-proc/up.sh
# AFTER its own .env, via SCARAB_ENV_EXTRA.
#
# 0.0.0.0, not 127.0.0.1: the Forgejo container delivers to the docker host, and
# a loopback-bound server is unreachable from it.
SCARAB_ADDR=0.0.0.0:$SCARAB_VERIFY_PORT
# The callback URL bind stamps onto the hook it registers — must be the address
# FORGEJO can reach, not the one a human types.
SCARAB_PUBLIC_URL=http://$FORGEJO_CALLBACK_HOST:$SCARAB_VERIFY_PORT
# The secret /webhooks/forgejo verifies with; the same one the adapter stamps
# into the hook it creates. Mismatch here = every delivery 401s.
SCARAB_FORGEJO_WEBHOOK_SECRET=$FORGEJO_WEBHOOK_SECRET
EOF
if [ "$tunnel" = 1 ]; then
  echo "SCARAB_LIBS_MANIFEST=$here/.libs-manifest.json" >> "$here/.env.scarab"
fi

echo "==> Forgejo ready at $base (admin: $owner)"
if [ "$tunnel" = 1 ]; then
  echo "    tunnel: $tunnel_url (admin password in $here/.env.generated)"
fi
