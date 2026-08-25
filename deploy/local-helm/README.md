# Local dogfood: Scarab-on-Scarab via Helm (colima)

Runs the full stack **in-cluster** on colima — in-cluster Postgres + MinIO +
`scarab-server` (which serves the web UI at `/`) — so we can repeatedly deploy a
pinned image and watch this repo's own CI run on Scarab. Docs (`scarab-docs-ui`)
are **not** deployed here; they ship to GitHub Pages.

**Object store (why MinIO):** the server's CAS holds logs, artifacts, and the
per-step **workspace snapshots** that reruns restore. `deploy.sh` points it at the
in-cluster MinIO (`minio.yaml`, official `minio/minio` image — no Bitnami, like
Postgres) on a PVC. This is load-bearing: the local-dir fallback lives on the
server Pod's `scratch` emptyDir, so — since every deploy now rolls the Pod — it
would wipe all workspaces and any rerun of a prior run would hang restoring its
input. Override `SCARAB_S3_*` in `.env` to use real S3 (then MinIO is skipped).

**Workspace service (ADR-0061):** `deploy.sh` also deploys the workspace service —
a StatefulSet running the **same image** with `SCARAB_ROLE=workspace`, holding a
warm content-addressed store of Workspace Snapshots on its own PVC, with MinIO
behind it as the cold archive. It is deployed **by default**, not behind a flag:
the ADR puts it in the standard path in every deployment mode, because a fast path
plus a fallback path is two mental models.

Its token secret is generated once into `deploy/local-helm/.workspace-token-secret`
(gitignored) rather than per deploy — the control plane mints tokens with it and
the service verifies them, so a value that rotated on every `just local-helm`
would invalidate every in-flight Step's credential mid-run and look exactly like
the service being down. It **must not** equal `SCARAB_RESULTS_TOKEN_SECRET`;
`deploy.sh` refuses if it does.

`deploy.sh` waits for `statefulset/scarab-workspace` to be Ready, which means its
PVC bound *and* its `/readyz` passed (warm writable + cold reachable). Deploying a
control plane whose data plane never came up is precisely the failure shape this
repo keeps finding.

> **Unverified:** this has not been deployed to colima in this change. The
> templates render and the precedence of `SCARAB_ROLE` was checked against a real
> kubelet, but no `just local-helm` run has exercised the StatefulSet, its PVC, or
> whether a rerun of a pre-restart Run now restores instead of hanging.

### `just local-helm` only works on an image that knows the workspace role

Three deliberate choices interact badly, and until ADR-0061 merges they rule out
every mode except `local`:

1. the workspace service runs the **same image** as the server, with
   `SCARAB_ROLE=workspace` (that is the anti-skew property);
2. it is deployed **unconditionally** — standard path, no flag;
3. its readiness is a **hard deploy gate**.

`edge` and `sha-*` are built by `image.yml` from `main`. A `scarab-server` from
before ADR-0061 has no `workspace` variant in its `--role` enum, so clap rejects
the value, the container exits 2, the Pod CrashLoopBackOffs, and
`kubectl rollout status statefulset/scarab-workspace` blocks for 180s before
timing out **without naming any of that**.

`deploy.sh` now **preflights** it, before touching a single cluster object. It
asks the image itself which roles it knows (`--role <nonsense>` makes clap print
its possible-values list — no config, no database, no network) and refuses with
the cause, the consequence, and the way out. It preflights the **fetcher** image
the same way, because that one fails differently and worse: a missing
`scarab-wsfetch` does not break the deploy at all, it leaves every Step that
inherits a workspace sitting in `Init:ImagePullBackOff` *after* the script has
printed "Deployed." `ghcr.io/thulasi-ram/scarab-wsfetch:edge` does not exist until
`image.yml` publishes it post-merge.

So, today:

```sh
just local-helm local        # ✅ builds server + clone + sidecar + wsfetch from the tree
just local-helm              # ❌ refused: `edge` predates ADR-0061 (and no wsfetch:edge yet)
just local-helm sha-<sha>    # ✅ only once image.yml has published ADR-0061
```

There is deliberately **no flag that disables the workspace service** to work
around this, and none should be added. ADR-0061 puts it in the standard path in
every deployment mode precisely because a fast path plus a fallback path is two
mental models — and the moment `workspace.enabled=false` becomes a documented
escape hatch, the dogfood stops exercising the thing being dogfooded. The
preflight's job is a clear error, not a fallback.

An operator installing `deploy/helm/scarab` directly owns the same check; the
symptom there is a CrashLoopBackOff on `statefulset/<release>-workspace` with the
clap error in the container log.

## Config — one file
Everything (image, App knobs, secrets, reseed inputs) lives in `deploy/local-helm/.env`
(gitignored). Both scripts source it; a real environment variable already set in
your shell overrides the file value.
```sh
cp deploy/local-helm/.env.example deploy/local-helm/.env
# fill in: SCARAB_MASTER_KEY (keep it STABLE across deploys, or stored secrets
#   become undecryptable), SCARAB_GITHUB_WEBHOOK_SECRET,
# SCARAB_RESULTS_TOKEN_SECRET, SCARAB_GITHUB_APP_ID, SCARAB_PUBLIC_URL (tunnel),
# SCARAB_APP_PEM (mounted at boot — see below), and the reseed inputs
#   (SCARAB_INSTALL_ID / ORG / REPO).
```
Context **must** be `colima` (deploy.sh refuses otherwise — never target EKS).

### NFS in the colima VM — a prerequisite of ADR-0062 stage **2b** only

Nothing in this stack needs an NFS client today. It becomes a prerequisite only for the
**NFS-delivered Workspace Export** (ADR-0062 stage 2b); the stage-2a `LocalPath` delivery
hands a co-located Step the Depot's overlay through a `local` PV with no network in it.

Probed 2026-08-02: the colima VM is Ubuntu 24.04.4 / kernel 6.8.0-117 / k3s v1.35.0+k3s1
and ships **no `mount.nfs`**, though the `nfs`/`nfsv4`/`nfsd` modules are on disk. One
command fixes it, and an NFS-backed PVC then mounts and round-trips on this cluster in
about six seconds:

```sh
colima ssh -- sudo apt-get update
colima ssh -- sudo apt-get install -y nfs-common   # mount.nfs 2.6.4 from ports.ubuntu.com
```

It survives `colima stop`/`start` and is **lost on `colima delete`**. There is no colima
provision hook in this repo, so this is a documented step rather than an automated one —
and the failure it prevents is silent-ish: kubelet reports *"you might need a
`/sbin/mount.nfs` helper program"* and the Pod sits in `ContainerCreating` **forever**.
`csi-driver-nfs` is the alternative for nodes where a package cannot be installed; see
ADR-0062's amended "no DaemonSet" note, including its `fsGroupPolicy` trap.

## GitHub App configuration (REQUIRED — both gaps fail *silently*)
The webhook URL + secret are not enough. Two App settings must be right or the
loop half-works with **no error anywhere**:

1. **Subscribe to events.** In the App's *Permissions & events → Subscribe to
   events*, check **Push** and **Pull request** (and **Status** if you want
   status-of-status). If this list is empty, `push`/`pull_request` are **never
   delivered**, so nothing triggers — yet `installation` /
   `installation_repositories` events still arrive (they are App-management
   events, independent of the subscription list), so the connection registers
   and `GET /v1/repos` looks healthy. The confusing symptom: *"the repo is
   registered but nothing ever runs."*
2. **Grant `statuses: write`.** *Permissions → Repository → Commit statuses:
   Read & write.* Without it, every commit-status post is rejected with HTTP
   403 `Resource not accessible by integration`. Scarab now logs this and
   dead-letters the status message after retries (it used to be dropped
   silently), but the run itself still succeeds — so you must fix the grant or
   the PR never shows a status. Also grant **Contents: read** (clone) and
   **Metadata: read**; add **Checks: read & write** only if using checks.

After changing permissions, GitHub emails an installation owner to **approve the
new access** — the change is not live until approved. Webhook URL is
`<SCARAB_PUBLIC_URL>/webhooks/github`, secret is `SCARAB_GITHUB_WEBHOOK_SECRET`.

## Image
- **Local (arm64, now):** `docker build -t scarab-server:dogfood-local -f docker/server/Dockerfile .`
  plus the clone/sidecar images (`docker build -t scarab-clone:dogfood docker/clone`,
  `... scarab-results-sidecar:dogfood docker/sidecar`). Keep `IMAGE_TAG=dogfood-local`.
- **GHA artifact (the real loop):** once `image.yml` publishes a multi-arch tag,
  set `IMAGE_REPOSITORY=ghcr.io/thulasi-ram/scarab-server` in `.env` and deploy by
  SHA — `deploy/local-helm/deploy.sh sha-<gitsha>`. (Needs the ghcr package public, or an
  `imagePullSecrets` entry.)

## Deploy / expose / seed
Convenience (recommended) — `just` picks the image source for you:
```sh
just local-helm             # pull + deploy the latest ghcr `edge` (pullPolicy Always)
just local-helm sha-<sha>   # pull + deploy a specific published SHA
just local-helm local       # build server+clone+sidecar+wsfetch from the tree, then deploy
```
Or drive the script directly (image source then comes from `.env`):
```sh
deploy/local-helm/deploy.sh [image-tag]         # postgres + helm upgrade --install
just local-helm-ui 8899                             # persistent forward (reconnects across deploys); cloudflared -> :8899
deploy/local-helm/reseed.sh                     # fresh DB only: register the installation
```
> **Every deploy rolls a fresh Pod.** Our tags are mutable — `edge` and
> `dogfood-local` never change string-wise — so a plain `helm upgrade` would
> render an identical Deployment and K8s would keep the *old* Pod running the
> stale image (`pullPolicy` only fires on container (re)creation). `deploy.sh`
> stamps a per-deploy `scarab.dev/deployed-at` Pod annotation, so Helm always
> rolls: the pull paths re-pull `edge`/`sha-*` fresh, and `local` adopts the
> just-rebuilt `dogfood-local`.
> **`just local-helm local` caveat:** it builds into the local **Docker** store
> and deploys with `pullPolicy: IfNotPresent`, so the images must be visible to
> the colima **Kubernetes** node. If a Pod reports `ErrImageNeverPull` /
> `ImagePullBackOff`, the node can't see the local build (colima's k8s reads
> containerd, not the Docker store) — prefer the ghcr path, or import the images
> into the node's store. The pull paths avoid this entirely (the node pulls from
> ghcr directly).
`deploy.sh` renders a transient Helm values file from `.env` (deleted on exit — no
secrets on the CLI or on disk). `reseed.sh` reads the webhook secret from the
deployed Secret, so it isn't written down anywhere.

### The App PEM is mounted, not seeded
`deploy.sh` puts `SCARAB_APP_PEM` into a k8s Secret (`scarab-github-app`) and the
chart mounts it at `/etc/scarab/forge/github-app.pem`, so the server has the App
credential **at boot** — no `POST /v1/secrets`, and it survives a DB wipe. It
overrides any DB-stored `_forge` credential, so `reseed.sh` detects the mount and
skips its PUT; what a fresh DB still needs from `reseed.sh` is the **installation
registration**, which is durable state, not a credential. Rotating the key is
`deploy.sh` again (the Secret is re-applied and the Pod rolls every deploy).

## Clean slate
```sh
helm uninstall scarab -n scarab
kubectl delete -n scarab -f deploy/local-helm/postgres.yaml   # drops the PVC => wipes the DB
kubectl delete -n scarab -f deploy/local-helm/minio.yaml      # drops the PVC => wipes the CAS
# next deploy.sh + reseed.sh starts fresh (deploy.sh recreates the bucket)
```
Wiping the DB PVC means re-running `reseed.sh` to re-register the installation. The App
PEM itself is unaffected — it is mounted from a Secret, not stored in Postgres (above).
Keep `SCARAB_MASTER_KEY` stable anyway, or any *other* previously-stored secret (org/env
secrets) becomes undecryptable. Wiping the MinIO PVC drops all logs/artifacts/workspace
snapshots — old runs stay in the DB but their content (and rerun inputs) is gone.
