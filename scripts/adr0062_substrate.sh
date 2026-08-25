#!/usr/bin/env bash
# Re-assert the substrate facts ADR-0062 rests on.
#
# ADR-0062 ("Workspace export: lazy materialisation without a node driver")
# does not choose its design from taste — it chooses it because a handful of
# kernel and Kubernetes behaviours are what they are. Each of those was measured
# once, by hand, on colima. Every one of them is a property of a *kernel version*
# or an *admission plugin*, so any of them can change under a colima/k3s bump and
# invalidate the ADR's reasoning **silently**.
#
# That is exactly the failure this repo has paid for before, so the measurements
# are a recipe rather than a paragraph. This script re-runs them and FAILS LOUDLY
# when a claim no longer holds, naming the ADR section that has to be rewritten.
#
# It asserts. It is not a demo and it prints no number it does not check.
#
# Run: just adr0062-substrate
set -euo pipefail

NS=default
POD_PREFIX=scarab-adr0062-probe
PASS=0
FAIL=0
declare -a FAILURES=()

ok()   { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m  %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); FAILURES+=("$1 -- $2"); printf '  \033[31mFAIL\033[0m  %s\n        expected: %s\n' "$1" "$2"; }
head_() { printf '\n\033[1m%s\033[0m\n' "$1"; }

cleanup() {
  kubectl delete pod -n "$NS" -l "adr0062probe=yes" --wait=false >/dev/null 2>&1 || true
  kubectl delete ns "${POD_PREFIX}-baseline" "${POD_PREFIX}-restricted" --wait=false >/dev/null 2>&1 || true
  rm -rf "$TMP" 2>/dev/null || true
}
TMP=$(mktemp -d)
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Guard. Production EKS contexts sit beside colima on developer machines; every
# probe below creates a privileged Pod, so a wrong context is not a small
# mistake. Exact match, no substring.
# ---------------------------------------------------------------------------
CTX=$(kubectl config current-context 2>/dev/null || echo "<none>")
if [ "$CTX" != "colima" ]; then
  echo "REFUSING: kube context is '$CTX', not exactly 'colima'." >&2
  echo "This probe creates privileged Pods. Switch context and re-run." >&2
  exit 2
fi
echo "context: $CTX"
kubectl get nodes -o jsonpath='{range .items[*]}node: {.metadata.name}  runtime: {.status.nodeInfo.containerRuntimeVersion}  kubelet: {.status.nodeInfo.kubeletVersion}{"\n"}{end}'

# ---------------------------------------------------------------------------
# 1. A hardlink shares its inode, so it shares mode and mtime.
#
# This is the fact that forced the Snapshot Farm onto reflink-or-copy: writing a
# snapshot's recorded metadata onto a hardlinked farm entry would mutate the CAS
# blob. Purely local (no cluster), and it is POSIX — if this ever fails, suspect
# the test, not the kernel. Asserted anyway because ADR-0062 originally claimed
# the opposite and shipped it for an hour.
# ADR-0062 § "Measured facts" / Alternatives ("A hardlink Farm").
# ---------------------------------------------------------------------------
head_ "1. hardlink metadata sharing (local)"
H="$TMP/hl"; mkdir -p "$H"
printf 'blob' > "$H/blob"; chmod 644 "$H/blob"; touch -t 202001010000 "$H/blob"
ln "$H/blob" "$H/farm-entry"
chmod 755 "$H/farm-entry"; touch -t 202507200000 "$H/farm-entry"
blob_mode=$(stat -f '%Lp' "$H/blob" 2>/dev/null || stat -c '%a' "$H/blob")
if [ "$blob_mode" = "755" ]; then
  ok "writing metadata via a hardlink mutates the other name (mode now $blob_mode)"
else
  bad "hardlink metadata sharing" "CAS blob mode to follow the farm entry to 755, got $blob_mode -- if hardlinks no longer share metadata, the Farm could use them and ADR-0062's Alternatives section is stale"
fi
# And the reflink contrast: independent metadata. macOS/APFS `cp -c`, Linux `cp --reflink`.
if cp -c "$H/blob" "$H/clone" 2>/dev/null || cp --reflink=always "$H/blob" "$H/clone" 2>/dev/null; then
  touch -t 201501010000 "$H/clone"
  cm=$(stat -f '%Sm' -t '%Y' "$H/clone" 2>/dev/null || stat -c '%y' "$H/clone" | cut -c1-4)
  bm=$(stat -f '%Sm' -t '%Y' "$H/blob"  2>/dev/null || stat -c '%y' "$H/blob"  | cut -c1-4)
  if [ "$cm" != "$bm" ]; then ok "reflink clone has independent metadata (clone $cm, blob $bm)"
  else bad "reflink independence" "clone and blob mtimes to differ, both $cm"; fi
else
  echo "  NOTE  reflink unavailable here -- the Farm takes the COPY rung on this filesystem (ADR-0062 Farm-build ladder). Not a failure; the copy rung is a supported configuration."
fi

# ---------------------------------------------------------------------------
# 2. Bidirectional mount propagation requires a privileged container, but a
#    privileged sidecar beside an UNPRIVILEGED restricted step container is a
#    valid Pod. This is what makes ADR-0062 part 5's accelerator possible at all
#    without weakening the step container. ADR-0062 § "Measured facts".
# ---------------------------------------------------------------------------
head_ "2. Bidirectional propagation requires privilege (and only in the sidecar)"
mk_bidir() { # $1 = privileged value for the sidecar
cat <<EOF
apiVersion: v1
kind: Pod
metadata: {name: ${POD_PREFIX}-bidir, labels: {adr0062probe: "yes"}}
spec:
  restartPolicy: Never
  volumes: [{name: ws, emptyDir: {}}]
  initContainers:
  - name: mounter
    image: busybox
    restartPolicy: Always
    securityContext: {privileged: $1}
    volumeMounts: [{name: ws, mountPath: /workspace, mountPropagation: Bidirectional}]
  containers:
  - name: step
    image: busybox
    securityContext:
      privileged: false
      runAsNonRoot: true
      runAsUser: 1000
      allowPrivilegeEscalation: false
      capabilities: {drop: [ALL]}
      seccompProfile: {type: RuntimeDefault}
    volumeMounts: [{name: ws, mountPath: /workspace, mountPropagation: HostToContainer}]
EOF
}
mk_bidir false > "$TMP/bidir-unpriv.yaml"; mk_bidir true > "$TMP/bidir-priv.yaml"
# NOTE: capture, never pipe. `kubectl apply` exits non-zero when admission
# rejects, and under `set -o pipefail` a `kubectl | grep -q` pipeline inherits
# that non-zero even when the grep matched -- which silently INVERTS every
# expect-a-rejection check. This script reported a false FAIL that way once.
dryrun() { kubectl apply --dry-run=server -n "$1" -f "$2" 2>&1 || true; }
if dryrun "$NS" "$TMP/bidir-unpriv.yaml" | grep -q "only to privileged containers"; then
  ok "unprivileged sidecar + Bidirectional is rejected"
else
  bad "Bidirectional/unprivileged" "rejection mentioning 'only to privileged containers' -- if this is now allowed, ADR-0062 part 5 no longer needs a privileged sidecar and the design gets BETTER; update the ADR"
fi
if dryrun "$NS" "$TMP/bidir-priv.yaml" | grep -q "dry run"; then
  ok "privileged sidecar + unprivileged restricted step container is a valid Pod"
else
  bad "Bidirectional/privileged" "the Pod to validate -- ADR-0062 part 5 is unbuildable if it does not"
fi

# ---------------------------------------------------------------------------
# 3. PodSecurity admission. This table is the reason the default path puts NO
#    privilege in the Step Pod and delivers the Export as a PVC:
#      - privileged is denied at BASELINE, not merely restricted
#      - inline `nfs:` is denied at restricted, a PVC is not
#    ADR-0062 § "Measured facts" (the PSA table) and part 2.
# ---------------------------------------------------------------------------
head_ "3. PodSecurity: what each level admits"
for lvl in baseline restricted; do
  ns="${POD_PREFIX}-${lvl}"
  kubectl create ns "$ns" >/dev/null 2>&1 || true
  kubectl label ns "$ns" "pod-security.kubernetes.io/enforce=${lvl}" --overwrite >/dev/null
done
psa_pod() { # $1 = name  $2 = volume block  $3 = extra spec
cat <<EOF
apiVersion: v1
kind: Pod
metadata: {name: $1, labels: {adr0062probe: "yes"}}
spec:
  restartPolicy: Never
  volumes:
$2
$3
  containers:
  - name: step
    image: busybox
    command: ["sh","-c","true"]
    securityContext:
      privileged: false
      runAsNonRoot: true
      runAsUser: 1000
      allowPrivilegeEscalation: false
      capabilities: {drop: [ALL]}
      seccompProfile: {type: RuntimeDefault}
    volumeMounts: [{name: ws, mountPath: /workspace}]
EOF
}
PRIV_SIDE='  initContainers:
  - name: mounter
    image: busybox
    restartPolicy: Always
    securityContext: {privileged: true}
    volumeMounts: [{name: ws, mountPath: /workspace, mountPropagation: Bidirectional}]'
psa_pod p-priv  '  - {name: ws, emptyDir: {}}'                                   "$PRIV_SIDE" > "$TMP/p-priv.yaml"
psa_pod p-nfs   '  - {name: ws, nfs: {server: 10.0.0.1, path: /export/x}}'       ''           > "$TMP/p-nfs.yaml"
psa_pod p-pvc   '  - {name: ws, persistentVolumeClaim: {claimName: probe-claim}}' ''          > "$TMP/p-pvc.yaml"

# case -> expected verdict per level ("allow" / "deny")
check_psa() { # $1 = level  $2 = file  $3 = expect  $4 = why-it-matters
  local out verdict
  # `|| true`: a rejected Pod exits non-zero, and under `set -e` a bare
  # command substitution of a failing command aborts the whole script.
  out=$(dryrun "${POD_PREFIX}-$1" "$2")
  if echo "$out" | grep -q "dry run"; then verdict=allow; else verdict=deny; fi
  if [ "$verdict" = "$3" ]; then ok "$1: $(basename "$2" .yaml) -> $verdict"
  else bad "$1: $(basename "$2" .yaml)" "$3, got $verdict -- $4"; fi
}
check_psa baseline   "$TMP/p-priv.yaml" deny  "if baseline now admits a privileged sidecar, the in-Pod mount could be the DEFAULT path and ADR-0062's central argument weakens"
check_psa restricted "$TMP/p-priv.yaml" deny  "same as above, at restricted"
check_psa baseline   "$TMP/p-nfs.yaml"  allow "inline nfs at baseline"
check_psa restricted "$TMP/p-nfs.yaml"  deny  "if restricted now admits inline nfs:, the per-Step PV+PVC churn in ADR-0062 part 2 is unnecessary ceremony and s6 gets simpler"
check_psa baseline   "$TMP/p-pvc.yaml"  allow "a PVC must be admissible or there is no default path"
check_psa restricted "$TMP/p-pvc.yaml"  allow "a PVC must be admissible at restricted or ADR-0062 part 2 is dead"

# ---------------------------------------------------------------------------
# 4. In-cluster kernel behaviour, in ONE privileged pod:
#      (a) a mount stacked on the shared volume's ROOT reaches an unprivileged
#          sibling -- this is why /workspace does not have to move
#      (b) overlayfs mounts over a hardlink farm on the node's filesystem, and
#          copy-up leaves the lower layer's blob UNCHANGED (the store cannot be
#          corrupted by a Step writing through its Export)
#      (c) index=on,nfs_export=on mounts (required to export it)
#      (d) metacopy=on does NOT give a metadata-only copy-up here, so it cannot
#          rescue a hardlink Farm
#    ADR-0062 § "Measured facts" and Open (the metacopy rejection).
# ---------------------------------------------------------------------------
head_ "4. kernel behaviour on the node (overlayfs, propagation, metacopy)"
cat > "$TMP/kernel.yaml" <<'EOF'
apiVersion: v1
kind: Pod
metadata: {name: scarab-adr0062-probe-kernel, labels: {adr0062probe: "yes"}}
spec:
  restartPolicy: Never
  volumes: [{name: d, emptyDir: {}}, {name: shared, emptyDir: {}}]
  initContainers:
  - name: mounter
    image: busybox
    restartPolicy: Always
    securityContext: {privileged: true}
    volumeMounts:
    - {name: shared, mountPath: /shared, mountPropagation: Bidirectional}
    command: ["sh","-c"]
    args: ["mount -t tmpfs tmpfs /shared && echo STACKED > /shared/SENTINEL && touch /tmp/ready && sleep 3600"]
    startupProbe: {exec: {command: ["test","-f","/tmp/ready"]}, periodSeconds: 1, failureThreshold: 30}
  containers:
  - name: t
    image: busybox
    securityContext: {privileged: true}
    volumeMounts:
    - {name: d, mountPath: /d}
    - {name: shared, mountPath: /shared, mountPropagation: HostToContainer}
    command: ["sh","-c"]
    args:
    - |
      echo "KERNEL=$(uname -r)"
      echo "NODEFS=$(stat -f -c %T /d)"
      # (a) did the sibling's root-stacked mount reach us?
      if [ -f /shared/SENTINEL ]; then echo "STACKED_PROPAGATED=yes"; else echo "STACKED_PROPAGATED=no"; fi
      mkdir -p /d/cas /d/farm/src /d/upper /d/work /d/merged /d/upper2 /d/work2
      echo "the blob content" > /d/cas/blob; chmod 644 /d/cas/blob; touch -t 202001010000 /d/cas/blob
      ln /d/cas/blob /d/farm/src/main.rs
      # (b) plain overlay over the farm; write through it; is the lower blob intact?
      if mount -t overlay overlay -o lowerdir=/d/farm,upperdir=/d/upper,workdir=/d/work /d/merged 2>/dev/null; then
        echo "OVERLAY_MOUNT=yes"
        echo "step wrote this" > /d/merged/src/main.rs
        echo "LOWER_INTACT=$([ "$(cat /d/cas/blob)" = 'the blob content' ] && echo yes || echo no)"
        echo "UPPER_HAS_ONLY_TOUCHED=$(cd /d/upper && find . -type f | tr '\n' ' ')"
        umount /d/merged
      else
        echo "OVERLAY_MOUNT=no"; echo "LOWER_INTACT=unknown"
      fi
      # (c) exportable form
      if mount -t overlay overlay -o lowerdir=/d/farm,upperdir=/d/upper,workdir=/d/work,index=on,nfs_export=on /d/merged 2>/dev/null; then
        echo "NFS_EXPORT_MOUNT=yes"; umount /d/merged
      else
        echo "NFS_EXPORT_MOUNT=no"
      fi
      # (d) metacopy. Use a BIG lower file: on a 17-byte file, ext4's 4 KiB
      # minimum allocation is indistinguishable from a real data copy, and
      # reading `size` (which metacopy preserves by design) as evidence of a copy
      # is how this script's first version reached the wrong conclusion. Compare
      # ALLOCATED BLOCKS against the file size instead.
      echo "METACOPY_PARAM=$(cat /sys/module/overlay/parameters/metacopy 2>/dev/null || echo absent)"
      mkdir -p /d/bigfarm/src
      dd if=/dev/zero of=/d/cas/bigblob bs=1M count=8 2>/dev/null
      chmod 644 /d/cas/bigblob; ln /d/cas/bigblob /d/bigfarm/src/big.bin
      if mount -t overlay overlay -o lowerdir=/d/bigfarm,upperdir=/d/upper2,workdir=/d/work2,index=on,metacopy=on /d/merged 2>/dev/null; then
        chmod 755 /d/merged/src/big.bin
        echo "METACOPY_UPPER_SIZE=$(stat -c %s /d/upper2/src/big.bin 2>/dev/null || echo missing)"
        echo "METACOPY_UPPER_BLOCKS=$(stat -c %b /d/upper2/src/big.bin 2>/dev/null || echo missing)"
        umount /d/merged
      else
        echo "METACOPY_UPPER_SIZE=mount-failed"; echo "METACOPY_UPPER_BLOCKS=mount-failed"
      fi
      # (e) THE fact that actually forbids metacopy for a Workspace Export: it is
      # mutually exclusive with nfs_export, and an Export must be exportable.
      # This, not the module parameter, is why the Farm cannot use hardlinks.
      rm -rf /d/upper3 /d/work3; mkdir -p /d/upper3 /d/work3
      if mount -t overlay overlay -o lowerdir=/d/farm,upperdir=/d/upper3,workdir=/d/work3,index=on,nfs_export=on,metacopy=on /d/merged 2>/tmp/mc; then
        echo "NFSEXPORT_PLUS_METACOPY=mounted"; umount /d/merged
      else
        echo "NFSEXPORT_PLUS_METACOPY=refused"
      fi
      # (f) rename(2) of an inherited DIRECTORY without redirect_dir. Build tools
      # do this constantly; EXDEV here is author-visible substrate knowledge.
      echo "REDIRECT_DIR_PARAM=$(cat /sys/module/overlay/parameters/redirect_dir 2>/dev/null || echo absent)"
      mkdir -p /d/rfarm/ld/sub && echo x > /d/rfarm/ld/sub/f
      rm -rf /d/upper4 /d/work4; mkdir -p /d/upper4 /d/work4
      if mount -t overlay overlay -o lowerdir=/d/rfarm,upperdir=/d/upper4,workdir=/d/work4,index=on,nfs_export=on /d/merged 2>/dev/null; then
        if mv /d/merged/ld /d/merged/nd 2>/tmp/mv; then echo "DIRRENAME_NO_REDIRECT=ok"; else echo "DIRRENAME_NO_REDIRECT=failed"; fi
        umount /d/merged
      else
        echo "DIRRENAME_NO_REDIRECT=mount-failed"
      fi
      rm -rf /d/upper5 /d/work5; mkdir -p /d/upper5 /d/work5
      if mount -t overlay overlay -o lowerdir=/d/rfarm,upperdir=/d/upper5,workdir=/d/work5,index=on,nfs_export=on,redirect_dir=on /d/merged 2>/dev/null; then
        echo "REDIRECTDIR_WITH_NFSEXPORT=mounted"; umount /d/merged
      else
        echo "REDIRECTDIR_WITH_NFSEXPORT=refused"
      fi
EOF
kubectl delete pod -n "$NS" scarab-adr0062-probe-kernel --ignore-not-found --wait=true >/dev/null 2>&1
kubectl apply -n "$NS" -f "$TMP/kernel.yaml" >/dev/null
for _ in $(seq 60); do
  ph=$(kubectl get pod -n "$NS" scarab-adr0062-probe-kernel -o jsonpath='{.status.phase}' 2>/dev/null || true)
  # `if`, not `[ ] || [ ] && break` -- the latter returns 1 when both tests fail,
  # which `set -e` treats as a fatal command and the wait becomes a single tick.
  if [ "$ph" = "Succeeded" ] || [ "$ph" = "Failed" ]; then break; fi
  sleep 2
done
OUT=$(kubectl logs -n "$NS" scarab-adr0062-probe-kernel -c t 2>&1 || true)
get() { echo "$OUT" | grep -m1 "^$1=" | cut -d= -f2- ; }
[ -n "$OUT" ] || bad "kernel probe" "the probe pod to produce output; it produced none"
echo "  kernel=$(get KERNEL) node fs=$(get NODEFS)"
[ "$(get STACKED_PROPAGATED)" = "yes" ] \
  && ok "a mount stacked on the volume ROOT reaches an unprivileged sibling (so /workspace stays /workspace)" \
  || bad "root-stacked propagation" "yes -- if this stops working, ADR-0062 part 5 must nest the mount and the authored workspace path MOVES"
[ "$(get OVERLAY_MOUNT)" = "yes" ] \
  && ok "overlayfs mounts over a hardlink farm on the node filesystem" \
  || bad "overlay mount" "yes -- ADR-0062 part 3's exact change set depends on it"
[ "$(get LOWER_INTACT)" = "yes" ] \
  && ok "copy-up left the lower layer's CAS blob unchanged (the store cannot be corrupted)" \
  || bad "lower-layer integrity" "yes -- this is the anti-corruption guarantee; if it fails, STOP and rewrite ADR-0062 part 1"
[ "$(get NFS_EXPORT_MOUNT)" = "yes" ] \
  && ok "index=on,nfs_export=on mounts (the exportable form)" \
  || bad "nfs_export overlay" "yes -- an Export cannot be served without it"
# metacopy: informational, NOT a pass/fail. It does work (an 8 MiB lower file
# copies up as metadata only), and the first version of this script concluded the
# opposite by reading `size` -- which metacopy preserves by design -- as evidence
# of a data copy. What decides the design is (e) below, not this.
MC_SZ=$(get METACOPY_UPPER_SIZE); MC_BLK=$(get METACOPY_UPPER_BLOCKS)
if [ "$MC_SZ" != "mount-failed" ] && [ "$MC_BLK" != "missing" ] && [ "$MC_BLK" -lt 1024 ] 2>/dev/null; then
  echo "  NOTE  metacopy IS effective here (8 MiB lower, upper size $MC_SZ but only $MC_BLK blocks allocated, param $(get METACOPY_PARAM)) -- so metacopy is available to part 5's NODE-side overlay, which carries no nfs_export"
else
  echo "  NOTE  metacopy ineffective or unavailable here (size $MC_SZ, blocks $MC_BLK, param $(get METACOPY_PARAM))"
fi
# THE load-bearing fact: an Export must be exportable, and nfs_export excludes
# metacopy. That is what forbids a hardlink Farm on the service side.
[ "$(get NFSEXPORT_PLUS_METACOPY)" = "refused" ] \
  && ok "nfs_export=on + metacopy=on is refused (conflicting options) -- so a Workspace Export cannot use metacopy, and the Farm cannot use hardlinks" \
  || bad "nfs_export+metacopy" "refused. It MOUNTED, which means an Export could combine them -- a hardlink Farm becomes viable on the service side and ADR-0062 part 1's Farm-build ladder must be revisited"
# rename(2) of an inherited directory: EXDEV without redirect_dir, and build
# tools do this constantly. redirect_dir=on is therefore not optional.
[ "$(get REDIRECTDIR_WITH_NFSEXPORT)" = "mounted" ] \
  && ok "redirect_dir=on coexists with nfs_export=on (unlike metacopy) -- the fix for inherited-directory renames is available" \
  || bad "redirect_dir+nfs_export" "the combination to mount. Without it an inherited directory rename returns EXDEV inside an Export, which is author-visible substrate knowledge the governing principle forbids"
DR=$(get DIRRENAME_NO_REDIRECT)
if [ "$DR" = "failed" ]; then
  echo "  NOTE  renaming an inherited directory FAILS without redirect_dir (param $(get REDIRECT_DIR_PARAM)) -- expected, and why redirect_dir=on is required. Note \`mv\` may mask this by copying the subtree, which silently turns the change set into a full copy of an inherited tree."
else
  echo "  NOTE  inherited-directory rename reported '$DR' without redirect_dir (param $(get REDIRECT_DIR_PARAM)) -- if \`mv\` fell back to a recursive copy this reads as success while re-ingesting the whole subtree"
fi

# ---------------------------------------------------------------------------
head_ "result"
printf '  %d passed, %d failed\n' "$PASS" "$FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf '\n\033[31mADR-0062 rests on assumptions that no longer hold:\033[0m\n'
  for f in "${FAILURES[@]}"; do printf '  - %s\n' "$f"; done
  printf '\nDo not "fix" this script to make it green. Read docs/adr/0062-*.md and\n'
  printf 'change the DECISION, or record why the new behaviour does not matter.\n'
  exit 1
fi
printf '\n  Every substrate fact ADR-0062 cites still holds on this cluster.\n'
