#!/usr/bin/env bash
# Assert a chart version bump never touches an immutable StatefulSet field.
#
# Kubernetes rejects any update to a StatefulSet's spec outside a short
# allowlist (replicas, ordinals, template, updateStrategy,
# persistentVolumeClaimRetentionPolicy, minReadySeconds). So a value that moves
# with the chart version anywhere else in `spec` — volumeClaimTemplates labels,
# selector, serviceName — turns every release into a forbidden update. That is
# exactly how the workspace PVC template broke every Argo CD sync after
# v0.1.0: its labels carried helm.sh/chart and app.kubernetes.io/version.
#
# Renders the chart at two different versions and diffs everything in each
# StatefulSet's spec except the allowlist. Run by image.yml's `chart` job.
#
# Usage: scripts/chart_statefulset_immutable.sh [chart-dir]   (needs helm, yq v4)
set -euo pipefail

chart=${1:-deploy/helm/scarab}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# The StatefulSets' spec minus the fields Kubernetes lets an update change.
# The workspace token switches the workspace StatefulSet on; defaults alone
# render none.
immutable() {
  helm template t "$1" --set secrets.workspaceTokenSecret=x |
    yq -o=json -I=2 'select(.kind == "StatefulSet")
      | {"name": .metadata.name,
         "spec": (.spec | del(.replicas, .ordinals, .template, .updateStrategy,
                              .persistentVolumeClaimRetentionPolicy, .minReadySeconds))}'
}

for v in 0.0.1 99.0.0; do
  cp -R "$chart" "$tmp/$v"
  yq -i ".version = \"$v\" | .appVersion = \"$v\"" "$tmp/$v/Chart.yaml"
  immutable "$tmp/$v" > "$tmp/$v.json"
done

# An empty render would compare equal and prove nothing.
if ! grep -q '"name"' "$tmp/0.0.1.json"; then
  echo "::error::rendered no StatefulSet — the check would pass vacuously" >&2
  exit 1
fi

if ! diff -u "$tmp/0.0.1.json" "$tmp/99.0.0.json"; then
  echo "::error::an immutable StatefulSet spec field changes with the chart version (diff above);" \
    "every upgrade would be rejected by the API server" >&2
  exit 1
fi
echo "ok: immutable StatefulSet spec fields are identical across chart versions"
