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
# granted. So both are granted here with `scarab-server admin grant`, run in
# the server Pod against its own database: Owner for the Owner, member for the
# member. A grant names a Principal, which exists once that person has signed
# in; until then the command exits 3 and this script says who has to sign in.
#
# --checklist also prints the one-time steps of a fresh database that need a
# passkey (the `production` Environment and the clone library). Without login
# up.sh did them with plain curl; with login they are crown-jewel operations.
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
    /opt/scarab/bin/scarab-server admin grant --org "$org" --subject "$1" --role "$2" \
    --reason "deploy/kind/seed.sh: dogfood $2" || rc=$?
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

if [ "${1:-}" = --checklist ]; then
  cat <<TXT

==> one-time steps on a fresh database (each asks for a passkey confirmation)
    1. Sign in at $ui_url as $dogfood_owner (use localhost, not 127.0.0.1:
       passkeys do not work on an IP address). Enroll a passkey in
       Settings > Security.
    2. Org settings > Environments: create \`production\` and make it eligible
       for all projects (the dogfood pipeline deploys to it).
    3. Org settings > Libraries: the clone library should pin scarab-clone:dev.
       If it is missing, choose "Paste the files" and paste
       deploy/kind/.state/dogfood-clone.yaml. If it pins another image,
       uninstall it first.
    4. To use the API, issue a token in Org settings > API tokens.
TXT
fi
