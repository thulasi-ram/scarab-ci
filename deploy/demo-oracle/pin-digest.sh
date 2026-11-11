#!/usr/bin/env bash
# pin_digest <image>: print the image reference pinned to the digest its tag
# names right now, as `<name>:<tag>@sha256:<digest>`. Sourced by deploy.sh.
#
# Why: a tag can move (a re-tagged release, `edge` on every push to main), and
# Kubernetes keys both its rollout decision and the node's image cache on the
# reference string. With an unchanged string, a StatefulSet does not roll, and a
# Step Pod with pullPolicy IfNotPresent starts from the node's old copy. A
# digest in the reference changes the string whenever the image changes, and
# names exactly one image. The tag stays in the reference so a human can read
# which release it is; the runtime pulls by the digest.
#
# Asks the registry directly (HEAD on the manifest, the Distribution API), with
# the anonymous Bearer token the registry's 401 offers, so it needs only curl
# and works for ghcr.io and Docker Hub alike. A reference that already carries a
# digest is printed unchanged. Fails (status 1, nothing on stdout) when the
# registry cannot be reached or does not know the tag.
#
# Run it by hand: bash deploy/demo-oracle/pin-digest.sh ghcr.io/thulasi-ram/scarab-server:0.3.2

pin_digest() {
  local ref="$1"
  case "$ref" in *@sha256:*) printf '%s\n' "$ref"; return 0 ;; esac

  local name tag
  case "${ref##*/}" in
    *:*) name="${ref%:*}"; tag="${ref##*:}" ;;
    *) name="$ref"; tag=latest ;;
  esac

  # The registry is the first path component when it looks like a host;
  # anything else is a Docker Hub name (`postgres` is `library/postgres`).
  local registry path
  registry="${name%%/*}"
  case "$name" in
    */*) ;;
    *) registry="" ;;
  esac
  case "$registry" in
    *.*|*:*|localhost) path="${name#*/}" ;;
    *) registry=registry-1.docker.io
       path="$name"
       case "$path" in */*) ;; *) path="library/$path" ;; esac ;;
  esac

  local url="https://$registry/v2/$path/manifests/$tag"
  local accept="application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json"

  local headers status
  headers="$(curl -sS -I --max-time 30 -H "Accept: $accept" "$url" | tr -d '\r')" || return 1
  status="$(printf '%s\n' "$headers" | awk 'NR==1 {print $2}')"

  if [ "$status" = 401 ]; then
    local challenge realm service scope token
    challenge="$(printf '%s\n' "$headers" | grep -i '^www-authenticate:' | head -1)"
    realm="$(printf '%s' "$challenge" | sed -n 's/.*realm="\([^"]*\)".*/\1/p')"
    service="$(printf '%s' "$challenge" | sed -n 's/.*service="\([^"]*\)".*/\1/p')"
    scope="$(printf '%s' "$challenge" | sed -n 's/.*scope="\([^"]*\)".*/\1/p')"
    [ -n "$realm" ] || return 1
    token="$(curl -sS --max-time 30 -G "$realm" \
      ${service:+--data-urlencode "service=$service"} \
      ${scope:+--data-urlencode "scope=$scope"} |
      sed -E -n 's/.*"(token|access_token)"[[:space:]]*:[[:space:]]*"([^"]+)".*/\2/p')" || return 1
    [ -n "$token" ] || return 1
    headers="$(curl -sS -I --max-time 30 -H "Accept: $accept" -H "Authorization: Bearer $token" "$url" | tr -d '\r')" || return 1
  fi

  local digest
  digest="$(printf '%s\n' "$headers" | awk -F': ' 'tolower($1) == "docker-content-digest" {print $2}' | tail -1)"
  case "$digest" in
    sha256:*) printf '%s:%s@%s\n' "$name" "$tag" "$digest" ;;
    *) return 1 ;;
  esac
}

# Run directly (not sourced): resolve each argument.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  set -euo pipefail
  [ "$#" -gt 0 ] || { echo "usage: $0 <image> [<image>…]" >&2; exit 2; }
  for image in "$@"; do
    pin_digest "$image" || { echo "could not resolve $image to a digest" >&2; exit 1; }
  done
fi
