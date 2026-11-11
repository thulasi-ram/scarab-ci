#!/usr/bin/env bash
# Install Scarab's clone and build libraries into an org of the kind stack,
# as an org admin installs any library, from the in-cluster Forgejo: no
# install reads github.com.
#
#   deploy/kind/libs.sh install <org>     # dev and e2e modes (up.sh runs it)
#   deploy/kind/libs.sh connection <org>  # print the id of <org>'s Forgejo
#                                         # connection, made if needed
#
# deploy/kind/forgejo.sh seeds `<org>/scarab-libs` (clone.yaml pinned to the
# locally built scarab-clone:dev image, build.yaml, notify.yaml,
# argocd/manifest.json) and
# records its commit in .state/libs/<org>.commit. An org reads a private
# source only through a connection of its own, and only its own
# repositories, so this:
#
#   1. makes a Forgejo connection for <org> at the in-cluster URL, with the
#      seed's API token. forgejo.sh revokes the previous run's token, so a
#      connection an earlier run left is deleted first. In e2e mode one that
#      serves Projects is deleted with them, as the e2e cases reclaim theirs;
#      in dev mode such a connection is kept and used, and the install is
#      skipped with a warning when its token no longer reads the source.
#   2. fetches `<host>/<org>/scarab-libs//clone.yaml@<commit>` through it,
#      installs it as `lib:thulasi-ram/clone` (or upgrades what is there),
#      and does the same for build.yaml as `lib:thulasi-ram/build`,
#   3. trusts clone.yaml's source with forge credentials (POST
#      …/library-trust), so a Step of the library receives the connection's
#      long-lived token: the stack has no GitHub App to mint one per Run;
#      and trusts build.yaml's source for the build sandbox, so its Step may
#      run rootless BuildKit. The build library is not trusted with the
#      registry credential or forge credentials: no pipeline of the stack
#      pushes.
#
# The calls carry no credential: dev and e2e modes run SCARAB_DEV_INSECURE,
# so step-up is off. Idempotent.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"
state="$here/.state"

need curl; need python3

cmd="${1:-}"
org="${2:-}"
case "$cmd" in
  install|connection) [ -n "$org" ] || die "usage: libs.sh $cmd <org>" ;;
  *) die "usage: libs.sh install <org> | libs.sh connection <org>" ;;
esac
mode="$(cat "$state/mode" 2>/dev/null || true)"
case "$mode" in
  dev|e2e) ;;
  *) die "libs.sh runs in dev or e2e mode (the release is in '${mode:-no}' mode)" ;;
esac
[ -f "$state/forgejo.env" ] || die "no $state/forgejo.env; forgejo.sh seeds it (just stack up)"
token="$(env_get "$state/forgejo.env" SCARAB_TEST_FORGEJO_TOKEN)"
server_url="$(env_get "$state/forgejo.env" SCARAB_TEST_FORGEJO_SERVER_URL)"
libs_repo="$(env_get "$state/forgejo.env" SCARAB_TEST_FORGEJO_LIBS_REPO)"
commit="$(cat "$state/libs/$org.commit" 2>/dev/null || true)"
[ -n "$commit" ] || die "forgejo.sh seeded no $org/$libs_repo (set SCARAB_DEV_ORG or PLUGIN_VERIFY_ORG before 'just stack up')"
# The host a source names: the connection's, without scheme or port.
host="$(python3 -I -c 'import sys,urllib.parse;print(urllib.parse.urlsplit(sys.argv[1]).hostname)' "$server_url")"
scarab_addr   # base

# json EXPR: evaluate EXPR over the JSON on stdin (`d`).
json() { python3 -I -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {'d': d}))" "$1"; }

# post/put/delete PATH [BODY]: the API, failing loudly with the server's text.
call() { # METHOD PATH [BODY]
  local out code
  out="$(mktemp "$state/libs-call.XXXXXX")"
  code="$(curl -s -o "$out" -w '%{http_code}' -X "$1" "$base$2" \
    -H 'content-type: application/json' ${3:+-d "$3"})"
  if [ "${code:0:1}" != 2 ]; then
    echo "error: $1 $2 answered HTTP $code: $(cat "$out")" >&2
    rm -f "$out"
    return 1
  fi
  cat "$out"
  rm -f "$out"
}

# ensure_connection: prints the id of <org>'s connection to the in-cluster
# Forgejo, made with this run's token.
ensure_connection() {
  local existing id projects
  existing="$(call GET /v1/connections | V_ORG="$org" V_URL="$server_url" python3 -I -c '
import json, os, sys
for c in json.load(sys.stdin):
    if c.get("org") == os.environ["V_ORG"] and c.get("kind") == "forgejo" \
       and c.get("base_url", "").rstrip("/") == os.environ["V_URL"].rstrip("/"):
        print(c["id"], len(c.get("projects") or []))
        break
')"
  if [ -n "$existing" ]; then
    id="${existing% *}"
    projects="${existing#* }"
    if [ "$projects" = 0 ] || [ "$mode" = e2e ]; then
      echo "    replacing connection $id of $org (its token was the previous run's)" >&2
      call DELETE "/v1/connections/$id?unbind_repos=true" >/dev/null
    else
      echo "    using connection $id of $org: it serves $projects Project(s), so it is kept" >&2
      printf '%s\n' "$id"
      return 0
    fi
  fi
  call POST /v1/connections \
    "$(V_ORG="$org" V_URL="$server_url" V_TOKEN="$token" python3 -I -c '
import json, os
print(json.dumps({"org": os.environ["V_ORG"], "kind": "forgejo",
                  "base_url": os.environ["V_URL"], "credential": os.environ["V_TOKEN"]}))')" \
    | json 'd["id"]'
}

conn="$(ensure_connection)"
if [ "$cmd" = connection ]; then
  printf '%s\n' "$conn"
  exit 0
fi

# install_lib NAME GRANT: fetch NAME.yaml from the seeded repo through the
# connection, install it as lib:thulasi-ram/NAME (or upgrade what is there),
# and trust its source with GRANT (forge_credentials or build_sandbox).
install_lib() {
  local name="$1" grant="$2" source fetched hash current replaces
  source="$host/$org/$libs_repo//$name.yaml@$commit"
  echo "==> installing lib:thulasi-ram/$name into $org from $source"
  if ! fetched="$(call POST "/v1/orgs/$org/libraries/fetch" \
        "{\"source\":\"$source\",\"connection\":\"$conn\",\"publisher\":\"thulasi-ram\",\"name\":\"$name\"}")"; then
    if [ "$mode" = dev ]; then
      echo "warning: could not read $source through connection $conn; the $name library is not installed in $org" >&2
      return 0
    fi
    die "could not read $source"
  fi
  hash="$(echo "$fetched" | json 'd["content_hash"]')"
  current="$(echo "$fetched" | json '(d.get("current") or {}).get("content_hash") or ""')"
  if [ "$current" != "$hash" ]; then
    replaces=null
    [ -z "$current" ] || replaces="\"$current\""
    call PUT "/v1/orgs/$org/libraries/thulasi-ram/$name" \
      "{\"content_hash\":\"$hash\",\"replaces\":$replaces}" >/dev/null
    echo "    installed ${hash:0:12}"
  else
    echo "    ${hash:0:12} is installed already"
  fi
  # Trust is per source and grant, without the commit: later seeds keep it.
  call POST "/v1/orgs/$org/library-trust" \
    "{\"source\":\"${source%@*}\",\"grant\":\"$grant\"}" >/dev/null
  echo "    trusted ${source%@*} with $grant"
}

# clone.yaml pins scarab-clone:dev.
install_lib clone forge_credentials
install_lib build build_sandbox
