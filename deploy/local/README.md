# Local dogfood: Scarab-on-Scarab via Helm (colima)

Runs the full stack **in-cluster** on colima — in-cluster Postgres + `scarab-server`
(which serves the web UI at `/`) — so we can repeatedly deploy a pinned image and
watch this repo's own CI run on Scarab. Docs (`scarab-docs-ui`) are **not** deployed
here; they ship to GitHub Pages.

## Config — one file
Everything (image, App knobs, secrets, reseed inputs) lives in `deploy/local/.env`
(gitignored). Both scripts source it; a real environment variable already set in
your shell overrides the file value.
```sh
cp deploy/local/.env.example deploy/local/.env
# fill in: SCARAB_MASTER_KEY (STABLE — reuse .env.local), SCARAB_GITHUB_WEBHOOK_SECRET,
# SCARAB_RESULTS_TOKEN_SECRET, SCARAB_GITHUB_APP_ID, SCARAB_PUBLIC_URL (tunnel),
# and the reseed inputs (SCARAB_APP_PEM / INSTALL_ID / ORG / REPO).
```
Context **must** be `colima` (deploy.sh refuses otherwise — never target EKS).

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
- **Local (arm64, now):** `docker build -t scarab-server:dogfood-local .` plus the
  clone/sidecar images (`docker build -t scarab-clone:dogfood deploy/clone`,
  `... scarab-results-sidecar:dogfood deploy/sidecar`). Keep `IMAGE_TAG=dogfood-local`.
- **GHA artifact (the real loop):** once `image.yml` publishes a multi-arch tag,
  set `IMAGE_REPOSITORY=ghcr.io/thulasi-ram/scarab-server` in `.env` and deploy by
  SHA — `deploy/local/deploy.sh sha-<gitsha>`. (Needs the ghcr package public, or an
  `imagePullSecrets` entry.)

## Deploy / expose / seed
```sh
deploy/local/deploy.sh [image-tag]              # postgres + helm upgrade --install
kubectl port-forward -n scarab svc/scarab 8899:80   # leave running; cloudflared -> :8899
deploy/local/reseed.sh                          # fresh DB only: store PEM + register
```
`deploy.sh` renders a transient Helm values file from `.env` (deleted on exit — no
secrets on the CLI or on disk). `reseed.sh` reads the webhook secret from the
deployed Secret, so it isn't written down anywhere.

## Clean slate
```sh
helm uninstall scarab -n scarab
kubectl delete -n scarab -f deploy/local/postgres.yaml   # drops the PVC => wipes the DB
# next deploy.sh + reseed.sh starts fresh
```
Because the App PEM lives in Postgres (encrypted under `SCARAB_MASTER_KEY`), wiping the
PVC means re-running `reseed.sh`. Keep `SCARAB_MASTER_KEY` stable or previously-stored
secrets become undecryptable.
