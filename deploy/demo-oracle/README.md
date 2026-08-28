# Public demo: Scarab on one Oracle "Always Free" box

A live, internet-reachable Scarab at `https://demo-scarab.<your-domain>`, running
on a single Oracle Cloud **Always Free** A1 instance — arm64, 2 OCPU, 12 GB RAM,
Ubuntu 24.04, 200 GB boot volume. That one machine is the whole deployment:
control plane, workspace service, Postgres, the tunnel, **and** every step Pod.

> **Status (2026-08-27): deployed and serving.** `https://demo-scarab.ahiravan.dev`
> returns 200, `/readyz` is 200 (Postgres *and* R2 reachable), `/v1/auth/login`
> 302s to the provider with PKCE `S256`, and the nightly `pg_dump` CronJob has
> written a dump to R2 on its own. Three things this directory assumed were
> wrong, and are fixed: see "What the first deploy found" below.
>
> Still **unproven**, in order of how likely they are to bite:
>
> 1. **R2 under real load.** The credential path is proven (list/put/get/delete
>    from inside the cluster, plus the backup CronJob), but the Depot's
>    multipart pack write path against R2 has not been exercised by an actual
>    drain — that needs a run with a workspace.
> 2. **The two-core budget.** `cargo check` in a step Pod on two shared Ampere
>    cores is still a guess, not a measurement. Nothing has run yet.
> 3. **Step Pods in their own namespace** (`scarab.namespace: scarab-steps`).
>    The chart supports it and renders the executor RBAC there, but no other
>    deployment mode in this repo uses a non-default exec namespace, and no step
>    Pod has been launched here.
>
> The OCI iptables repair is no longer on this list: the live ruleset was read
> off the instance and matches what `bootstrap.sh` targets, and pod networking
> was smoke-tested (cluster DNS, service-CIDR routing, egress to ghcr.io).

## What runs where

| | |
|---|---|
| **k3s** | installed on the host by `bootstrap.sh`, with `traefik`, `servicelb` and `metrics-server` disabled and **local-storage kept** (both PVCs need it) |
| **ingress** | `cloudflared.yaml` — a Deployment that dials *out* to Cloudflare. No ingress controller, no Ingress object, no cert-manager, no LoadBalancer, **no public inbound port on the box at all**. Cloudflare terminates TLS |
| **object store** | Cloudflare **R2**, not MinIO. State must live off the box: Oracle can reclaim the instance |
| **database** | in-cluster Postgres on local-path (`postgres.yaml`), dumped to R2 nightly |
| **auth** | real GitHub OAuth. `DEV_INSECURE` is refused by `deploy.sh` |
| **step Pods** | namespace `scarab-steps`, capped by a LimitRange (`steps.yaml`) |

Files: `bootstrap.sh` (one-time host prep) → `.env` → `deploy.sh` (everything
else, idempotent, re-runnable). `just demo-oracle` is the entry point.

## Before you can deploy

Four things must exist first. Each fails differently and none of them fail in a
way `deploy.sh` can diagnose for you.

1. **The instance.** Oracle Cloud → Always Free → Ampere A1, 2 OCPU / 12 GB,
   Ubuntu 24.04, 200 GB boot volume. The OCI security list needs **SSH only** —
   the tunnel is egress-only, so nothing else is opened, ever.
2. **A Cloudflare tunnel.** Zero Trust → Networks → Tunnels → create a
   *remotely-managed* tunnel. Copy its token into `CLOUDFLARE_TUNNEL_TOKEN`.
   On the tunnel's **Public Hostname** tab add
   `demo-scarab.<domain>` → `http://scarab.scarab.svc.cluster.local:80`.
   The routing lives there and cannot live in `cloudflared.yaml` — token mode
   has no config file.
3. **An R2 bucket** plus an API token scoped to it (Object Read & Write).
   Endpoint is `https://<account_id>.r2.cloudflarestorage.com` — account id, not
   bucket name. Region is the literal string `auto`.
   **Add the lifecycle rule** (bucket → Settings → Object lifecycle rules →
   *Abort multipart uploads* → 1 day). This is not optional: the Depot streams
   drain bytes as multipart pack uploads and aborts its own on error and on
   graceful shutdown, but a *crashed* replica cannot, and an S3-compatible store
   gives a server no way to list uploads it does not know about — so the leaked
   parts bill forever (git-bug ad79c90). MinIO did this natively; R2 does not.
4. **GitHub credentials** — two separate things:
   * a **GitHub App** (clones, commit statuses). Webhook URL
     `https://demo-scarab.<domain>/webhooks/github`, secret =
     `SCARAB_GITHUB_WEBHOOK_SECRET`. It must **subscribe to Push and Pull
     request** events and be granted **Contents: read**, **Metadata: read**,
     **Commit statuses: read & write** — the two failure modes here are silent
     and are documented at length in `deploy/local-helm/README.md`;
   * a **GitHub OAuth app** (visitor login). Authorization callback URL
     `https://demo-scarab.<domain>/v1/auth/callback`.

## Running it

```sh
# on the box, once
sudo bash deploy/demo-oracle/bootstrap.sh
curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash

cp deploy/demo-oracle/.env.example deploy/demo-oracle/.env   # then fill it in
just demo-oracle                # deploy ghcr `edge`
just demo-oracle sha-<gitsha>   # deploy a specific published build
```

`bootstrap.sh` is idempotent; so is `deploy.sh`. Re-run either freely.

**There is no local-build path**, unlike `just local-helm local`. Two Ampere
cores will not compile this workspace in a time anyone will wait for, and doing
it on the demo box would starve the running demo of both cores while it
happened. `image.yml` builds every image on native amd64 **and arm64** runners
and stitches the digests into one multi-arch manifest per tag, so `edge` and
`sha-*` already carry an arm64 manifest — pull, never build.

### Fresh install: registering the App installation

Install the GitHub App on the demo repo (or hit *Recreate all* on its Advanced →
webhook page). The resulting `installation` delivery is what registers it.
Unlike `deploy/local-helm` there is **no `reseed.sh`** here and none is needed:
the box is publicly reachable, so real GitHub deliveries arrive on their own.

The App **PEM** is a different matter and is handled: `deploy.sh` puts
`SCARAB_APP_PEM` into a k8s Secret the chart mounts, so the server holds the
credential at boot and it survives a DB wipe or restore.

## What the first deploy found

Three defects, all fixed, recorded because each one is invisible until you run
this mode specifically.

**`kubectl` did not work as `ubuntu`.** `/usr/local/bin/kubectl` is a symlink to
the k3s binary, and that wrapper prefers `/etc/rancher/k3s/k3s.yaml` over
`$HOME/.kube/config` and does **not** fall back when it cannot read it — so a
root-only kubeconfig broke kubectl for the very user `deploy.sh` runs as, even
with a perfectly good copy in their home directory. `bootstrap.sh` now passes
`--write-kubeconfig-group`. Rejected the one-character alternative
(`--write-kubeconfig-mode 0644`): world-readable cluster-admin credentials on a
public box.

**Re-running `bootstrap.sh` poisoned `/etc/iptables/rules.v4`.**
`netfilter-persistent save` snapshots the *live* ruleset, so running it once k3s
was up baked 79 lines of kube-proxy / kube-router / flannel runtime chains into
the file — including stale `kube-dns … has no endpoints -j REJECT` rules, which
`netfilter-persistent` then restores **at boot, before k3s starts**. kube-proxy
full-syncs and heals it, but a persisted DNS reject is precisely the failure
class that section exists to prevent. The save is now skipped when k3s is
already installed.

**The chart did not give the workspace StatefulSet the OAuth client secret.**
Fixed in the chart, not here (`deploy/helm/scarab/templates/statefulset-workspace.yaml`).
The Pod saw four of five `SCARAB_OAUTH_*` values and refused to boot, since
ADR-0049 makes a provider all-five-or-none and ADR-0048 validates config before
the role is dispatched. Unreachable from `deploy/local-helm`, which runs
`SCARAB_DEV_INSECURE=true`; this mode is the first with authentication actually
on. Fixing it exposed a second problem now guarded in `deploy.sh`: a
StatefulSet's RollingUpdate will not replace a Pod that never became Ready, so
the corrected template changed nothing until the Pod was deleted by hand.

### Fixes applied directly to the running box

Every fix above is in `bootstrap.sh`, so a **fresh** box gets them from the
script. The box that already exists was repaired **in place**, because bootstrap
had already run on it. The two states are equivalent; they were reached by
different routes, and that is worth knowing before you debug the live host.

| What | On a fresh box | On the box that exists |
|---|---|---|
| kubeconfig group | `--write-kubeconfig-group` at install time | `--write-kubeconfig-group ubuntu` added to `ExecStart` in `/etc/systemd/system/k3s.service`, `daemon-reload`, `systemctl restart k3s` |
| `rules.v4` | never saved after k3s is up | rewritten once: every `KUBE-*`/`FLANNEL-*`/`CNI-*`/`cali*` chain declaration, every rule in those chains, and every builtin rule jumping to one were stripped; validated with `iptables-restore --test` before install. What remains is the base ruleset — Oracle's 17-line `InstanceServices` chain, the SSH accept, both trailing REJECTs, and the eight pod/service ACCEPTs |
| UDP buffers for QUIC | `/etc/sysctl.d/99-scarab-udp-buffers.conf` | same file, written by hand |

Consequences to keep in mind:

* `--write-kubeconfig-group` is an **install-time** flag baked into the systemd
  unit. If you ever re-run `get.k3s.io` on this host, re-check `ExecStart` —
  a reinstall can rewrite the unit and silently take the group back off.
* Do **not** replay these by hand on a rebuilt box. Run `bootstrap.sh`; it does
  all three, in the right order, before k3s exists.

## The operational facts that matter

### Two cores is the constraint, not the RAM

Rough budget of the 12 GB:

| | |
|---|---|
| k3s + system | ~0.8 G |
| `scarab-server` | 0.5 G limit |
| workspace service | 0.25 G request / 1 G limit |
| Postgres | 0.5 G limit |
| cloudflared | 0.128 G limit |
| **left for step Pods** | **~8 G** |

`steps.yaml` caps a step at 2 GiB by default and 4 GiB by request, and gives
every container a 250m CPU **request** so the scheduler queues step Pods rather
than packing two cores past what fits. Deliberately **no CPU limit** — see the
comments in that file; a CPU limit throttles the wsfetch drain's hashing, and
the step's own wall-clock deadline (`stepTimeoutSecs: 1800`) is the real bound.

`bootstrap.sh` adds a 4 GiB swap file. It is a safety net for two specific
spikes — an in-Pod `cargo check`, and the drain hashing and uploading
`/workspace` — where without it the kernel OOM-kills a step, the Attempt
classifies **Transient**, and the retry burns both cores again on a run that
dies the same way.

### Retention is aggressive, on purpose

Set through `scarab.extraEnv` (the chart has no stanza for these; the names are
`SCARAB_RETENTION_*` from `crates/scarab-server/src/config.rs`):

| class | here | chart/server default |
|---|---|---|
| logs | 7d | 30d |
| artifacts | 7d | 90d |
| workspace CAS | 3d | 14d |
| Depot packs | 3d | = workspace TTL |

Run **metadata** is kept regardless — old runs stay in the UI, their content
does not. Packs must be ≥ the workspace TTL (they back Workspace Snapshots) or
the server refuses to boot; 3/3 is the tightest legal pair.

`casConcurrency` is raised to **96** from the default 32, because R2 is a remote
bucket and the CAS legs are latency-bound — the chart's own comment calls 32 a
*floor* for remote object storage. The counter-pressure is memory: peak is
roughly `casConcurrency × largest blob`. Raise it further only alongside the
server's memory limit.

### Oracle will reclaim an idle instance

The published rule is a **conjunction** over a 7-day window at the 95th
percentile: CPU under 20% **and** network under 20% **and** memory under 20%. An
idle k3s box fails CPU and network comfortably; 12 GB means memory is the only
one that might save it alone, and "probably fine" is not a plan for the box the
demo lives on.

`.github/workflows/demo-keepalive.yml` runs every 4 hours and pushes an empty
commit to a `demo-keepalive` branch, which the GitHub App delivers as a real
`push` → a real Run. Six real runs a day move all three metrics.

It has to be an outside poke, because **nothing in the engine schedules `cron:`
triggers**. The DSL parses `on: { cron: … }` and `scarab_forge::Event::Cron`
exists as a type, but outside `scarab-forge`'s own unit tests `Event::Cron`
appears only in `match` arms — nothing constructs one. A pipeline declaring a
cron trigger simply never runs.

### There is no machine credential for `POST …/dispatch`

The keepalive would rather dispatch than push, and today it structurally cannot:

* requests authenticate with `Authorization: Bearer <session-id>` (or the
  `scarab_session` cookie) — a **session id** is the only bearer credential the
  server knows (`session_id()` in `crates/scarab-server/src/lib.rs`);
* sessions are minted in exactly one place, `POST /v1/auth/login`, which takes an
  OAuth **authorization code** and exchanges it at the provider
  (`OAuthAuthenticator::exchange`). Authorization codes are single-use and only
  a browser redirect produces one;
* a session lives **24 hours** (`SESSION_TTL_MS`);
* there is no PAT, no service account, no token-issuing endpoint, and no
  client-credentials grant. `openapi.json` declares no `securitySchemes` at all.

So a scheduled runner has nothing to present, and a hand-pasted session would
expire five times between two runs of a 4-hourly cron. Closing this is a **server
feature** — issued API tokens with a verb and an expiry, minted by an Owner — not
a deploy trick, and the workflow says so in a marked TODO rather than inventing
one. What it does instead is not a workaround: a real push through the real
webhook is a *better* demo, and nothing is forged.

### Backups, and what surviving instance loss actually means

`postgres.yaml` carries a CronJob at 03:17 UTC: `pg_dump -Fc` into an emptyDir,
then `aws s3 cp` to R2 at `pg/scarab-<ISO-day-of-week>.dump`. **Seven rotating
slots** — the eighth night overwrites the first — so the retention policy needs
no bucket lifecycle rule and no prune job that could delete the wrong thing.

Restore:

```sh
aws s3 cp s3://$BUCKET/pg/scarab-3.dump ./scarab.dump \
  --endpoint-url https://<account_id>.r2.cloudflarestorage.com
kubectl cp ./scarab.dump scarab/<postgres-pod>:/tmp/scarab.dump
kubectl exec -n scarab <postgres-pod> -- \
  pg_restore -U scarab -d scarab --clean --if-exists /tmp/scarab.dump
```

So, if Oracle reclaims the instance:

* **survives** — logs, artifacts, Depot packs, workspace snapshots (all in R2),
  and up to 24h-old run history (the nightly dump);
* **lost** — up to a day of run history, every in-flight run, and the warm CAS
  (a cache: it re-fills from R2, at the cost of cache misses);
* **must be replayed** — `bootstrap.sh`, `.env` (keep it somewhere else!), and
  the App installation registration.

**Verified working (2026-08-27):** the CronJob ran on its own and left
`pg/scarab-5.dump` in the bucket, which exercises the day-of-week rotation and a
second, independent R2 writer besides the control plane.

Two values in `.env` are the ones you cannot lose: `SCARAB_MASTER_KEY` (change it
and every stored secret becomes undecryptable) and, if you keep the DB,
`.workspace-token-secret`.

> ⚠ The dump job's `aws-cli` container sets
> `AWS_REQUEST_CHECKSUM_CALCULATION=when_required` and
> `AWS_RESPONSE_CHECKSUM_VALIDATION=when_required`. Recent aws-cli v2 sends a
> CRC32 full-object checksum on every upload that R2 rejects
> (`Header 'x-amz-checksum-crc32' … not implemented`); without these two the
> backup fails every night with an error that reads like a credential problem
> and is not.

### Security posture

`DEV_INSECURE` is **refused** by `deploy.sh` before anything is applied, and the
chart refuses to render it alongside `scarab.oauth` anyway. On a box with a real
hostname it would make every caller — including every anonymous one — a
synthetic Owner able to dispatch runs and read stored secrets.

Everyone who signs in lands as **Viewer**: read-only, cannot dispatch.
`SCARAB_OAUTH_OWNERS` is the maintainer's login and is the only path to Owner.
Leave it empty and nobody can administer the install, including you.

This rests on **two assumptions**: the demo repo is *public*, and the demo is
*login-gated*. Together they are why admitting any GitHub user as a Viewer is
safe — a Viewer reads every run's logs and artifacts, which are the repo's source
and build output. `DEMO_ASSUME_PUBLIC_REPO=false` in `.env` makes `deploy.sh`
refuse, because `scarab-server` has **no sign-in allowlist**: `SCARAB_OAUTH_OWNERS`
elevates a login to Owner, it does not restrict who may log in. There is no
value that makes a private repo safe here; that would be a server feature (a
subject allowlist at `authenticate()`).

### Why in-cluster Postgres on a box that can be reclaimed

Neon's free tier cannot back this: 100 compute-hours/month against a server that
holds a connection pool open permanently is about four days of uptime, and then
the demo is down. So Postgres lives on local-path and the **bucket** is the
durable copy (above). This is a demo posture, not a production one — production
uses managed Postgres.

### Why step Pods get their own namespace

A LimitRange applies to every Pod in its namespace; there is no way to scope one
to "just the step Pods". In the release namespace it would also hit the workspace
StatefulSet, and the default it injects is exactly what the chart warns against
there — a CPU limit throttles the drain's hashing, and the drain clock is already
the liveness bound. Two namespaces is what lets step Pods be capped without
capping the data plane with them.

The proper home for this is ADR-0055's `default_resources`, and it is not
reachable: `SCARAB_PLACEMENT_CONFIG_FILE` is file-only (no inline env form) and
the chart has no value that mounts an arbitrary file. Wiring it would be a chart
change. If the chart grows one, move the caps there and delete `steps.yaml` —
`default_resources` is per-step and legible in the server's own config, where a
LimitRange is invisible to Scarab entirely.

## Clean slate

```sh
helm uninstall scarab -n scarab
kubectl delete -n scarab -f deploy/demo-oracle/postgres.yaml   # drops the PVC => wipes the DB
kubectl delete -n scarab -f deploy/demo-oracle/cloudflared.yaml
kubectl delete -f deploy/demo-oracle/steps.yaml
# the workspace PVC is a StatefulSet volumeClaimTemplate — helm does not remove it
kubectl delete pvc -n scarab -l app.kubernetes.io/component=workspace
```

Wiping the DB means re-registering the App installation (reinstall it on the
repo). The App PEM is unaffected — it is mounted from a Secret, not stored in
Postgres. Keep `SCARAB_MASTER_KEY` stable regardless, or any *other* previously
stored secret becomes undecryptable. The R2 bucket is untouched by all of this;
emptying it drops every log, artifact, pack and workspace snapshot, and old runs
then exist in the DB with no content behind them.
