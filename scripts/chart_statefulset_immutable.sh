#!/usr/bin/env bash
# Assert a chart version bump never touches an immutable StatefulSet field.
#
# Kubernetes rejects any update to a StatefulSet's spec outside a short
# allowlist (replicas, ordinals, template, updateStrategy,
# persistentVolumeClaimRetentionPolicy, minReadySeconds — the 1.30 list; 1.33+
# also allows revisionHistoryLimit, kept immutable here because older clusters
# still reject it). So a value that moves
# with the chart version anywhere else in `spec` — volumeClaimTemplates labels,
# selector, serviceName — turns every release into a forbidden update. That is
# exactly how the workspace PVC template broke every Argo CD sync after
# v0.1.0: its labels carried helm.sh/chart and app.kubernetes.io/version.
#
# Renders the chart at two versions that differ in every component, under both
# the default values and a features-on set (so a version behind a non-default
# branch is caught too), and diffs everything in each StatefulSet's spec except
# the allowlist. Run by image.yml's `chart` job and `just chart-immutable`.
#
# Out of reach by construction: a value that changes per RELEASE rather than per
# chart version (`.Release.Revision`, `now`) renders the same in both passes.
# Keep those out of immutable fields by review.
#
# Usage: scripts/chart_statefulset_immutable.sh [chart-dir]   (needs helm, yq v4)
set -euo pipefail

chart=${1:-deploy/helm/scarab}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# Two values sets. The workspace token switches the workspace StatefulSet on
# (defaults alone render none); `features` also takes the non-default branches.
values() {
  case $1 in
    default) echo --set secrets.workspaceTokenSecret=x ;;
    features) echo --set secrets.workspaceTokenSecret=x \
      --set workspace.persistence.storageClass=fast --set secrets.existingSecret=mine ;;
  esac
}

# Each StatefulSet's spec minus the fields Kubernetes lets an update change,
# plus the chart label — which MUST differ between versions, or the version
# rewrite did not take and the comparison proves nothing.
render() {
  local dir=$1; shift
  helm template t "$dir" "$@" |
    yq 'select(.kind == "StatefulSet")
      | {"name": .metadata.name,
         "chart": .metadata.labels."helm.sh/chart",
         "spec": (.spec | del(.replicas, .ordinals, .template, .updateStrategy,
                              .persistentVolumeClaimRetentionPolicy, .minReadySeconds))}'
}

for v in 1.2.3 98.97.96; do
  cp -R "$chart" "$tmp/$v"
  yq -i ".version = \"$v\" | .appVersion = \"$v\"" "$tmp/$v/Chart.yaml"
done

for set in default features; do
  for v in 1.2.3 98.97.96; do
    # shellcheck disable=SC2046 # word-splitting the --set flags is the point
    render "$tmp/$v" $(values "$set") > "$tmp/$set-$v.yaml"
  done
  # An empty render would compare equal and prove nothing.
  if ! grep -q '^name:' "$tmp/$set-1.2.3.yaml"; then
    echo "::error::[$set values] rendered no StatefulSet — the check would pass vacuously" >&2
    exit 1
  fi
  if [ "$(yq '.chart' "$tmp/$set-1.2.3.yaml")" = "$(yq '.chart' "$tmp/$set-98.97.96.yaml")" ]; then
    echo "::error::[$set values] the chart version did not change between renders" >&2
    exit 1
  fi
  if ! diff -u <(yq 'del(.chart)' "$tmp/$set-1.2.3.yaml") \
               <(yq 'del(.chart)' "$tmp/$set-98.97.96.yaml"); then
    echo "::error::[$set values] an immutable StatefulSet spec field changes with the chart" \
      "version (diff above); every upgrade would be rejected by the API server" >&2
    exit 1
  fi
done
echo "ok: immutable StatefulSet spec fields are identical across chart versions"
