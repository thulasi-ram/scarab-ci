#!/usr/bin/env bash
# One-time host prep for the PUBLIC demo box: an Oracle Cloud "Always Free" A1
# instance (arm64, 2 OCPU / 12 GB, Ubuntu 24.04) that runs BOTH the Scarab
# control plane and every step Pod. Run it ON the box:
#
#   git clone https://github.com/thulasi-ram/scarab-ci && cd scarab-ci
#   sudo bash deploy/demo-oracle/bootstrap.sh
#
# Idempotent — re-running it is a no-op on anything already done. It installs
# k3s, opens the two cluster CIDRs through Oracle's stock firewall, and adds
# swap. It deploys NOTHING: `deploy/demo-oracle/deploy.sh` (or `just
# demo-oracle`) does that, as an unprivileged user, afterwards.
#
# ⚠ UNVERIFIED: no line of this has been run against a real Oracle instance.
#   See deploy/demo-oracle/README.md.
set -euo pipefail

[ "$(id -u)" = "0" ] || { echo "run me as root (sudo bash $0)" >&2; exit 1; }

# The whole file assumes aarch64 Ubuntu. It is not a portability guard for its
# own sake: the OCI firewall repair below reads Oracle's stock Ubuntu ruleset,
# and the k3s flags assume the Ampere A1 shape. On anything else the commands
# would "succeed" against rules that are not there and leave a box that looks
# prepared and is not.
ARCH="$(uname -m)"
[ "$ARCH" = "aarch64" ] || {
  echo "refusing: this box is $ARCH, not aarch64." >&2
  echo "  demo-oracle targets an Oracle Always Free A1 (Ampere/arm64) instance." >&2
  exit 1
}

# ---------------------------------------------------------------------------
# 1. Swap.
#
# 12 GB of RAM is not the constraint here — the two Ampere cores are — but two
# things on this box spike memory in a way an OOM-killer resolves badly:
#   * a step Pod running `cargo check` (the demo pipeline does exactly that);
#   * the workspace drain, which hashes and uploads /workspace in-Pod.
# Without swap the kernel picks a victim, and when the victim is a step
# container the Attempt classifies Transient and RETRIES — burning the cores
# again on a run that will die the same way. Swap turns that cliff into a slow
# patch. Oracle's boot volume is 200 GB, so 4 GiB costs nothing.
#
# Note `vm.swappiness=10`: swap is a safety net here, not a tier we want the
# kernel reaching for while there is free RAM.
# ---------------------------------------------------------------------------
SWAPFILE=/swapfile
SWAP_SIZE_MB="${SWAP_SIZE_MB:-4096}"
if [ -f "$SWAPFILE" ]; then
  echo "==> swap: $SWAPFILE already exists — leaving it alone"
else
  echo "==> swap: creating ${SWAP_SIZE_MB}MiB at $SWAPFILE"
  # fallocate is instant on ext4 but produces a file some kernels refuse to
  # swapon (extent holes); dd is slower and always works. 4 GiB of dd on this
  # box is a few seconds.
  dd if=/dev/zero of="$SWAPFILE" bs=1M count="$SWAP_SIZE_MB" status=none
  chmod 600 "$SWAPFILE"
  mkswap "$SWAPFILE" >/dev/null
fi
swapon --show=NAME --noheadings | grep -qx "$SWAPFILE" || swapon "$SWAPFILE"
grep -q "^$SWAPFILE " /etc/fstab || echo "$SWAPFILE none swap sw 0 0" >> /etc/fstab
printf 'vm.swappiness=10\n' > /etc/sysctl.d/99-scarab-swappiness.conf
sysctl -q -p /etc/sysctl.d/99-scarab-swappiness.conf

# ---------------------------------------------------------------------------
# 2. inotify limits.
#
# k3s + kubelet + containerd + every Pod's log tail all take inotify watches,
# and Ubuntu's defaults are sized for a laptop. Exhausting them does not fail
# loudly: Pods hang in ContainerCreating and controllers stop seeing changes,
# with "too many open files" buried in a journal nobody is reading.
# ---------------------------------------------------------------------------
# cloudflared carries every byte of this demo over QUIC (UDP), and quic-go wants
# a ~7 MiB receive buffer. Ubuntu's default net.core.rmem_max is 208 KiB, so
# without this the tunnel logs
#   failed to sufficiently increase receive buffer size (was: 208 kiB, wanted: 7168 kiB)
# on every start and runs with a throughput ceiling — which on this deployment
# is the ceiling on streaming step logs to the UI. Not fatal, just quietly slow.
echo "==> sysctl: UDP buffers for the tunnel's QUIC transport"
cat > /etc/sysctl.d/99-scarab-udp-buffers.conf <<'EOF'
net.core.rmem_max=7500000
net.core.wmem_max=7500000
EOF
sysctl -q -p /etc/sysctl.d/99-scarab-udp-buffers.conf

echo "==> sysctl: inotify limits"
cat > /etc/sysctl.d/99-scarab-inotify.conf <<'EOF'
fs.inotify.max_user_instances=1024
fs.inotify.max_user_watches=524288
EOF
sysctl -q -p /etc/sysctl.d/99-scarab-inotify.conf

# ---------------------------------------------------------------------------
# 3. The OCI firewall repair — the one that costs a day if you skip it.
#
# Oracle's stock Ubuntu images ship an iptables ruleset (persisted in
# /etc/iptables/rules.v4 via iptables-persistent) whose INPUT and FORWARD
# chains end in:
#
#     -A INPUT   -j REJECT --reject-with icmp-host-prohibited
#     -A FORWARD -j REJECT --reject-with icmp-host-prohibited
#
# That is fine for the public interface and fatal for k3s: pod traffic
# (10.42.0.0/16) and Service traffic (10.43.0.0/16) traverse those same chains
# on the host, so they are REJECTed. The symptom is not "no network" — it is
# subtler and much more expensive to chase: CoreDNS never becomes Ready, every
# Pod's DNS times out after ~5s, the workspace service's /readyz fails against
# its own cold store, and `kubectl get nodes` says Ready throughout.
#
# The repair is to ACCEPT both CIDRs BEFORE the REJECT, in both directions.
# Inserted at position 1 so it is ahead of the reject regardless of what else
# the image put in the chain.
#
# NOT opened here, deliberately: nothing inbound from the internet. The
# Cloudflare Tunnel is egress-only (cloudflared dials OUT to Cloudflare), so
# this box needs no public listener at all — no 80, no 443, no NodePort, and
# the OCI security list can stay at SSH-only. If you find yourself opening a
# port to make the demo reachable, the tunnel is misconfigured.
# ---------------------------------------------------------------------------
POD_CIDR=10.42.0.0/16      # k3s default cluster-cidr (flannel)
SVC_CIDR=10.43.0.0/16      # k3s default service-cidr
echo "==> iptables: allowing $POD_CIDR and $SVC_CIDR past Oracle's stock REJECT"
for chain in INPUT FORWARD; do
  for cidr in "$POD_CIDR" "$SVC_CIDR"; do
    for dir in -s -d; do
      # -C tests for the rule; only insert when absent, so this is idempotent
      # and repeated runs do not stack duplicates at the top of the chain.
      iptables -C "$chain" "$dir" "$cidr" -j ACCEPT 2>/dev/null \
        || iptables -I "$chain" 1 "$dir" "$cidr" -j ACCEPT
    done
  done
done

# Persist. Without this the rules are correct until the first reboot, and the
# box then comes back with the failure above and no recent change to blame.
# ONLY on a box where k3s is not up yet. `netfilter-persistent save` snapshots
# the WHOLE live ruleset, so running it while k3s is up bakes kube-proxy's,
# kube-router's and flannel's runtime chains into rules.v4 — including, if the
# snapshot lands early in a restart, rules like
#   -A KUBE-SERVICES -d 10.43.0.10/32 ... "kube-dns:dns has no endpoints" -j REJECT
# which is then RESTORED AT BOOT, before k3s starts, by netfilter-persistent.
# kube-proxy full-syncs KUBE-SERVICES on start so it heals, but a persisted
# DNS-reject is precisely the class of bug this whole section exists to prevent.
# The four ACCEPTs above were already persisted by the first run; a re-run has
# nothing new to save.
if ! command -v netfilter-persistent >/dev/null 2>&1; then
  :
elif command -v k3s >/dev/null 2>&1; then
  echo "    k3s is already installed — skipping the save (rules.v4 holds the"
  echo "    base ruleset from the first run; saving now would bake in k3s's"
  echo "    runtime chains)"
else
  netfilter-persistent save >/dev/null
  echo "    saved via netfilter-persistent"
fi
if ! command -v netfilter-persistent >/dev/null 2>&1; then
  cat >&2 <<'EOF'
⚠ netfilter-persistent is not installed, so the rules just added will be LOST
  on reboot — and the box will come back with CoreDNS timing out and nothing
  recent to blame. Fix it now:

      sudo apt-get update && sudo DEBIAN_FRONTEND=noninteractive \
        apt-get install -y iptables-persistent
      sudo netfilter-persistent save
EOF
fi

# ---------------------------------------------------------------------------
# 4. k3s.
#
# Disabled: traefik (the tunnel replaces every reason to have an ingress
# controller — see cloudflared.yaml), servicelb (there is no LoadBalancer to
# provision and no public IP to attach), metrics-server (nothing here reads it;
# it is pure resident memory on a box whose steps want the RAM).
#
# NOT disabled, and this matters: local-storage. The k3s local-path provisioner
# is the default StorageClass, and it backs BOTH PVCs this deployment has — the
# workspace service's warm CAS (ADR-0061) and Postgres. `--disable
# local-storage` leaves both PVCs Pending forever, which surfaces as a
# StatefulSet that never becomes Ready and a deploy that times out on it.
# ---------------------------------------------------------------------------
# The group the kubeconfig is written to. Resolved HERE, before the install,
# because --write-kubeconfig-group is an INSTALL-time flag baked into the k3s
# systemd unit — there is no fixing it afterwards without editing the unit and
# restarting. Getting it wrong is not cosmetic: /usr/local/bin/kubectl is a
# symlink to the k3s binary, and that wrapper prefers /etc/rancher/k3s/k3s.yaml
# over $HOME/.kube/config and does NOT fall back when it cannot read it. So a
# root-only kubeconfig makes `kubectl` fail for the very user deploy.sh runs as,
# even though a perfectly good copy sits in their home directory.
TARGET_USER="${SUDO_USER:-}"
if [ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ]; then
  TARGET_GROUP="$(id -gn "$TARGET_USER")"
else
  TARGET_GROUP=root
fi

if command -v k3s >/dev/null 2>&1; then
  echo "==> k3s already installed ($(k3s --version | head -1)) — skipping install"
else
  echo "==> installing k3s (traefik/servicelb/metrics-server off, local-storage ON)"
  curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC="server \
    --disable traefik \
    --disable servicelb \
    --disable metrics-server \
    --write-kubeconfig-mode 0640 \
    --write-kubeconfig-group $TARGET_GROUP" sh -
fi
systemctl is-active --quiet k3s || systemctl start k3s

# Hand the invoking (non-root) user a kubeconfig. `deploy.sh` deliberately runs
# unprivileged — it should not need sudo to talk to the cluster, and a root-only
# kubeconfig is how people end up running the whole deploy as root.
if [ -n "$TARGET_USER" ] && [ "$TARGET_USER" != "root" ]; then
  TARGET_HOME="$(getent passwd "$TARGET_USER" | cut -d: -f6)"
  install -d -o "$TARGET_USER" -g "$TARGET_USER" -m 0700 "$TARGET_HOME/.kube"
  install -o "$TARGET_USER" -g "$TARGET_USER" -m 0600 \
    /etc/rancher/k3s/k3s.yaml "$TARGET_HOME/.kube/config"
  echo "==> kubeconfig copied to $TARGET_HOME/.kube/config (owner $TARGET_USER)"
fi

# Wait for the API to actually serve before claiming success — "installed" and
# "usable" are two different states and only one of them is worth printing.
echo "==> waiting for the node to become Ready"
# Two waits, not one. The k3s installer returns as soon as systemd reports the
# unit active, which is BEFORE the node object is registered — and
# `kubectl wait --all` does not wait on an empty set, it exits 1 immediately
# with "no matching resources found". So poll for the object to exist first,
# then wait on its condition.
for _ in $(seq 60); do
  KUBECONFIG=/etc/rancher/k3s/k3s.yaml k3s kubectl get nodes \
    -o name >/dev/null 2>&1 && break
  sleep 2
done
KUBECONFIG=/etc/rancher/k3s/k3s.yaml k3s kubectl wait --for=condition=Ready node --all --timeout=180s

cat <<'EOF'

Host prepared. Next, as the NON-root user:

  1. install helm (the deploy needs it; k3s does not ship it)
       curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash
  2. cp deploy/demo-oracle/.env.example deploy/demo-oracle/.env  && fill it in
  3. just demo-oracle          # or: bash deploy/demo-oracle/deploy.sh

Read deploy/demo-oracle/README.md first — the Cloudflare tunnel, the R2 bucket
and the GitHub OAuth app all have to exist before step 3 can work.
EOF
