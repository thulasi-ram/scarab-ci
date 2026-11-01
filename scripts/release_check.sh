#!/usr/bin/env bash
# What `just release <version>` checks before the owner tags v<version>
# (git-bug cb31124, docs/RELEASING.md). Every check runs, and the summary
# lists each one as PASS or FAIL; the exit code is 1 when any failed.
#
#   versions     Cargo.toml workspace.package.version, Chart.yaml version and
#                appVersion all equal <version> (the check image.yml runs on
#                the tag, before it pushes an image)
#   clone pin    libs/clone.yaml pins one scarab-clone image by digest, and
#                not the <version> build: a release cannot pin the image it
#                builds itself (scripts/pin_clone_image.sh, ADR-0076 §9)
#   changelog    CHANGELOG.md has a [<version>] section; prints the Release
#                text release.yml will write from it
#   docs links   every New entry under [Unreleased] and [<version>] links a
#                docs page that exists (scripts/changelog.py links)
#   screenshots  a fresh capture of the mock UI matches every committed
#                screenshot (ui/scarab-web-ui/scripts/screenshots.mjs check)
#
# Run from the repository root. Needs python3, node and npm; the first run
# downloads Playwright's Chromium.
set -euo pipefail

version="${1:-}"
version="${version#v}"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "usage: just release <version>   (e.g. 0.3.0)" >&2
  exit 2
fi

results=()
failed=0
record() { # record <PASS|FAIL> <name> <detail>
  results+=("$(printf '  %-4s  %-11s  %s' "$1" "$2" "$3")")
  [ "$1" = PASS ] || failed=1
}
section() { printf '\n==> %s\n' "$1"; }

section "versions"
chart=$(sed -n 's/^version:[[:space:]]*//p' deploy/helm/scarab/Chart.yaml | tr -d '"')
app=$(sed -n 's/^appVersion:[[:space:]]*//p' deploy/helm/scarab/Chart.yaml | tr -d '"')
cargo=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' Cargo.toml)
echo "want=$version cargo=$cargo chart=$chart appVersion=$app"
bad=()
[ "$cargo" = "$version" ] || bad+=("Cargo.toml is $cargo")
[ "$chart" = "$version" ] || bad+=("Chart.yaml version is $chart")
[ "$app" = "$version" ] || bad+=("Chart.yaml appVersion is $app")
if [ ${#bad[@]} -eq 0 ]; then
  record PASS versions "Cargo.toml, Chart.yaml version and appVersion are $version"
else
  record FAIL versions "$(IFS=';'; echo "${bad[*]}" | sed 's/;/; /g')"
fi

section "clone pin"
pins=$(grep -E '^[[:space:]]*image:[[:space:]]*ghcr\.io/thulasi-ram/scarab-clone' libs/clone.yaml || true)
echo "${pins:-(no scarab-clone image line)}"
count=$(printf '%s' "$pins" | grep -c . || true)
pin=$(printf '%s' "$pins" | sed -E 's/^[[:space:]]*image:[[:space:]]*//')
tag=$(printf '%s' "$pin" | sed -nE 's#^ghcr\.io/thulasi-ram/scarab-clone:([^@]+)@sha256:[0-9a-f]{64}$#\1#p')
if [ "$count" != 1 ]; then
  record FAIL "clone pin" "libs/clone.yaml has $count scarab-clone image lines, want 1"
elif [ -z "$tag" ]; then
  record FAIL "clone pin" "libs/clone.yaml does not pin scarab-clone:<tag>@sha256:<digest>: $pin"
elif [ "$tag" = "$version" ] || [ "$tag" = "v$version" ]; then
  record FAIL "clone pin" "libs/clone.yaml pins the $version build, which this release builds; pin an earlier one"
else
  record PASS "clone pin" "scarab-clone:$tag by digest"
fi

section "changelog"
if notes=$(python3 -I scripts/changelog.py notes "$version"); then
  printf '%s\n' "$notes"
  record PASS changelog "[$version] gives a $(printf '%s\n' "$notes" | wc -l | tr -d ' ')-line Release text"
else
  record FAIL changelog "CHANGELOG.md has no [$version] section; release.yml fails on the tag without it"
fi

section "docs links"
if links=$(python3 -I scripts/changelog.py links "$version" 2>&1); then
  printf '%s\n' "$links"
  record PASS "docs links" "$(printf '%s\n' "$links" | tail -1)"
else
  printf '%s\n' "$links"
  record FAIL "docs links" "$(printf '%s\n' "$links" | tail -1)"
fi

section "screenshots"
npm --prefix ui/scarab-web-ui install --no-audit --no-fund >/dev/null
npx --prefix ui/scarab-web-ui playwright install chromium >/dev/null
if shots=$(node ui/scarab-web-ui/scripts/screenshots.mjs check 2>&1); then
  printf '%s\n' "$shots"
  record PASS screenshots "$(printf '%s\n' "$shots" | tail -1)"
else
  printf '%s\n' "$shots"
  record FAIL screenshots "$(printf '%s\n' "$shots" | tail -1); \`just release screenshots\` recaptures them"
fi

printf '\nrelease check for %s\n' "$version"
printf '%s\n' "${results[@]}"
if [ "$failed" = 0 ]; then
  printf '\nall checks passed: tag v%s from main (docs/RELEASING.md)\n' "$version"
fi
exit "$failed"
