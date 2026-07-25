#!/usr/bin/env bash
# Tear the verification Forgejo down and delete its data volume (git-bug 3863d5e).
# The instance is throwaway by design — every run seeds it from scratch.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "==> removing the verification Forgejo (container + volume)"
docker compose -f "$here/compose.yaml" down -v --remove-orphans

# The generated env files carry a live API token — do not leave them lying about.
rm -f "$here/.env.generated" "$here/.env.scarab"
