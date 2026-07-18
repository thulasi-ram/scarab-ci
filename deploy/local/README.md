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
