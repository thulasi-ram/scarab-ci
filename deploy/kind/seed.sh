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
# passkey (the `production` Environment and the clone and build libraries).
# Without login up.sh did them with plain curl; with login they are
# crown-jewel operations. Without --checklist, the clone and build libraries
# installed in the org are compared with libs/clone.yaml and libs/build.yaml
# in this checkout (read-only, with psql in the Postgres Pod), and the step
# that installs each one that differs is printed.
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

# Scarab's clone and build libraries, installed like any library from their
# sources on github.com at the last commit that changed each. Dogfood runs in
# GitHub App mode against real GitHub, so reading a library from github.com
# is what a real install does (the dev and e2e modes install from the
# in-cluster Forgejo instead, deploy/kind/libs.sh). The clone library's token
# is minted per Run, so its source needs no trust. The build library's Step
# runs rootless BuildKit, so its source must be trusted for the build
# sandbox; the dogfood pipeline does not push, so it needs no trust with
# forge credentials. The commit must be pushed: the server reads the file
# from GitHub.
library_commit() { # $1 the library's name
  git -C "$here/../.." log -1 --format=%H -- "libs/$1.yaml" 2>/dev/null || true
}
library_step() { # $1 the step's number, $2 the library's name
  local commit
  commit="$(library_commit "$2")"
  cat <<TXT
    $1. Org settings > Libraries > Install: fetch
       github.com/thulasi-ram/scarab-ci//libs/$2.yaml@${commit:-<commit>}
       review it and install it under its default name,
       thulasi-ram::scarab-ci::$2 (from github.com: this stack runs against
       real GitHub). If another version is installed, this upgrades it.
TXT
}
build_source=github.com/thulasi-ram/scarab-ci//libs/build.yaml
trust_step() { # $1 the step's number
  cat <<TXT
    $1. Org settings > Libraries > thulasi-ram::scarab-ci::build > Build sandbox: trust
       $build_source, or Runs that build an image are refused. The dogfood
       pipeline does not push (the library's push Step exits without
       pushing), so it needs no registry-credential trust.
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
  library_step 3 clone
  library_step 4 build
  trust_step 5
  echo "    6. To use the API, issue a token in Org settings > API tokens."
else
  # Each installed version's content hash (SHA-256 of the file, ADR-0076),
  # read with psql in the Postgres Pod, against libs/<name>.yaml here. The
  # org and the name go in as psql variables, never spliced into the SQL.
  step=1
  for name in clone build; do
    want="$(shasum -a 256 "$here/../../libs/$name.yaml" | awk '{print $1}')"
    if have="$(printf '%s\n' "SELECT content_hash FROM library_installations
        WHERE org = :'org' AND name = 'thulasi-ram::scarab-ci::' || :'name';" \
      | kc exec -i -n "$ns" deploy/scarab-postgres -- \
          psql -U scarab -d scarab_dogfood -v ON_ERROR_STOP=1 -v org="$org" \
            -v name="$name" -tA)"; then
      if [ "$have" != "$want" ]; then
        if [ -z "$have" ]; then
          echo "==> the $name library is not installed in $org (needs a passkey confirmation)"
        else
          commit="$(library_commit "$name")"
          echo "==> the $name library installed in $org is not libs/$name.yaml at ${commit:-HEAD} (needs a passkey confirmation)"
        fi
        library_step "$step" "$name"
        step=$((step + 1))
      fi
    else
      echo "warning: could not read the $name library installed in $org; check Org settings > Libraries" >&2
    fi
  done
  # The build library's source needs the build-sandbox grant whatever version
  # is installed: checked on its own, not only when the version differs.
  if have="$(printf '%s\n' "SELECT count(*) FROM library_source_grants
      WHERE org = :'org' AND source = :'source' AND trust_grant = 'build_sandbox';" \
    | kc exec -i -n "$ns" deploy/scarab-postgres -- \
        psql -U scarab -d scarab_dogfood -v ON_ERROR_STOP=1 -v org="$org" \
          -v source="$build_source" -tA)"; then
    if [ "$have" = 0 ]; then
      echo "==> $org does not trust $build_source for the build sandbox (needs a passkey confirmation)"
      trust_step "$step"
    fi
  else
    echo "warning: could not read the trust of $build_source in $org; check Org settings > Libraries" >&2
  fi
fi
