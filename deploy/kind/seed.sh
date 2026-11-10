#!/usr/bin/env bash
# Seed the dogfood stack's members and print what is left to do by hand.
# `just stack up dogfood` runs it after the roll-out; `just stack seed dogfood`
# runs it again once the people below have signed in.
#
#   deploy/kind/seed.sh [--checklist]
#
# The dogfood stack runs real GitHub login (git-bug 2199c2e). Its Owner
# (DOGFOOD_OWNER, default github:$SCARAB_ORG) is an Owner at every login
# through scarab.oauth.owners, but the access page lists stored role bindings
# only, and the member (DOGFOOD_MEMBER, optional) has no role until one is
# granted. So both are granted here with `scarab-server admin grant
# --if-absent`, run in the server Pod against its own database: Owner for the
# Owner, member for the member. --if-absent writes only when the person has no
# binding in the org at all (a revoked one counts), so a role the Owner changed
# or removed in the UI stays as the Owner left it. A grant names a Principal,
# which exists once that person has signed in; until then the command exits 3
# and this script says who has to sign in.
#
# --checklist also prints the one-time steps of a fresh database that need a
# passkey (the `production` Environment and the clone library). Without login
# up.sh did them with plain curl; with login they are crown-jewel operations.
# Without --checklist, the clone library installed in the org is compared
# with libs/clone.yaml in this checkout (read-only, with psql in the Postgres
# Pod), and the step that installs it is printed when they differ.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=deploy/kind/lib.sh
. "$here/lib.sh"
dogfood_env="${SCARAB_DOGFOOD_ENV:-$HOME/.config/scarab/dogfood.env}"
state="$here/.state"

[ -f "$dogfood_env" ] || die "$dogfood_env does not exist (see deploy/kind/README.md)"
[ "$(cat "$state/mode" 2>/dev/null)" = dogfood ] \
  || die "the stack is not in dogfood mode; run 'just stack up dogfood' first"
org="$(env_get "$dogfood_env" SCARAB_ORG)"
[ -n "$org" ] || die "SCARAB_ORG is empty or missing in $dogfood_env"
dogfood_principals "$dogfood_env"   # dogfood_owner, dogfood_member
scarab_addr                         # host_port
ui_url="http://localhost:$host_port"

pending=""
grant() { # $1 principal, $2 role
  local rc=0
  kc exec -n "$ns" deploy/"$release" -c scarab-server -- \
    /opt/scarab/bin/scarab-server admin grant --if-absent --org "$org" --subject "$1" \
    --role "$2" --reason "deploy/kind/seed.sh: dogfood $2" || rc=$?
  case "$rc" in
    0) ;;
    3) pending="$pending $1" ;;
    *) die "granting $2 to $1 in $org failed (exit $rc); see the message above" ;;
  esac
}
echo "==> role bindings in $org"
grant "$dogfood_owner" owner
if [ -n "$dogfood_member" ]; then
  grant "$dogfood_member" member
else
  echo "    DOGFOOD_MEMBER is not set in $dogfood_env: no member to grant"
fi
for p in $pending; do
  echo "    sign in as $p at $ui_url, then run \`just stack seed dogfood\`"
done

# Scarab's clone library, installed like any library from its source on
# github.com at the last commit that changed it. The dogfood stack is in
# GitHub App mode, so its token is minted per Run and the source needs no
# trust. The commit must be pushed: the server reads the file from GitHub.
clone_commit="$(git -C "$here/../.." log -1 --format=%H -- libs/clone.yaml 2>/dev/null || true)"
library_step() { # $1 the step's number
  cat <<TXT
    $1. Org settings > Libraries > Install: fetch
       github.com/thulasi-ram/scarab-ci//libs/clone.yaml@${clone_commit:-<commit>}
       review it and install it as thulasi-ram/clone. If another version is
       installed, this upgrades it.
TXT
}

if [ "${1:-}" = --checklist ]; then
  cat <<TXT

==> one-time steps on a fresh database (each asks for a passkey confirmation)
    1. Sign in at $ui_url as $dogfood_owner (use localhost, not 127.0.0.1:
       passkeys do not work on an IP address). Enroll a passkey in
       Settings > Security.
    2. Org settings > Environments: create \`production\` and make it eligible
       for all projects (the dogfood pipeline deploys to it).
TXT
  library_step 3
  echo "    4. To use the API, issue a token in Org settings > API tokens."
else
  # The installed version's content hash (SHA-256 of the file, ADR-0076),
  # read with psql in the Postgres Pod, against libs/clone.yaml here. The org
  # goes in as a psql variable, never spliced into the SQL.
  want="$(shasum -a 256 "$here/../../libs/clone.yaml" | awk '{print $1}')"
  if have="$(printf '%s\n' "SELECT content_hash FROM library_installations
      WHERE org = :'org' AND publisher = 'thulasi-ram' AND name = 'clone';" \
    | kc exec -i -n "$ns" deploy/scarab-postgres -- \
        psql -U scarab -d scarab_dogfood -v ON_ERROR_STOP=1 -v org="$org" -tA)"; then
    if [ "$have" != "$want" ]; then
      if [ -z "$have" ]; then
        echo "==> the clone library is not installed in $org (needs a passkey confirmation)"
      else
        echo "==> the clone library installed in $org is not libs/clone.yaml at ${clone_commit:-HEAD} (needs a passkey confirmation)"
      fi
      library_step 1
    fi
  else
    echo "warning: could not read the clone library installed in $org; check Org settings > Libraries" >&2
  fi
fi
