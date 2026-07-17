# Handoff — dogfood: Scarab builds scarab-ci itself

The Arc A/B/C backlog is done (all `ready-for-agent` tickets closed on
`feat/arc-abc-impl`). The next proof is **dogfooding**: this repo's CI runs on
Scarab — a real GitHub App, a real webhook, a real clone with a minted token,
real Pods on a local cluster, statuses posted back to the PR. This exercises
every "HUMAN VERIFY" comment left on the closed tickets in one loop.

## Human setup (one-time, ~15 min — needs a browser)

1. **GitHub App** (github.com → Settings → Developer settings → GitHub Apps → New):
   - Webhook URL: your tunnel URL (step 2) + `/webhooks/github`; set a
     **webhook secret** (keep it).
   - Permissions: Contents **read & write**, Checks/Commit statuses **write**,
     Metadata read, Pull requests read. (Packages **write** too if you want the
     GHCR derived-credential path.)
   - Subscribe to events: Push, Pull request, Installation, Installation repositories.
   - Generate a **private key** (PEM) and note the **App ID**.
   - **Install** the App on `thulasi-ram/scarab-ci`.
2. **Webhook tunnel** to your laptop: `smee.io` channel or
   `cloudflared tunnel --url http://localhost:8080` — GitHub must reach
   `POST /webhooks/github`.
3. Colima with kubernetes: `colima start --kubernetes` (context **must** be
   `colima` — the kubeconfig also holds prod EKS contexts; never target them).
4. Local Postgres (`postgres://thulasiram@localhost:5432/postgres` works).

## The AFK prompt

Paste this into a fresh session:

```
Dogfood Scarab on the scarab-ci repo itself, per docs/handoffs/dogfood-scarab-on-scarab.md. I have created a GitHub App (I'll paste the App ID, webhook secret, and PEM path when you ask) and I have a webhook tunnel URL. Steps: (1) assert kubectl current-context is colima (NEVER the Acme EKS contexts); (2) build + make available in the cluster the three first-party images (scarab-clone, scarab-results-sidecar from deploy/, and pick digests) — docker build is visible to colima k3s directly; (3) boot scarab-server (converged, SCARAB_EXECUTOR=k8s, SCARAB_NAMESPACE=default) with: SCARAB_GITHUB_APP_ID, SCARAB_GITHUB_WEBHOOK_SECRET, SCARAB_CLONE_IMAGE + SCARAB_SIDECAR_IMAGE (local tags), SCARAB_RESULTS_TOKEN_SECRET + SCARAB_RESULTS_API_URL=http://192.168.5.2:8080 (pods reach the mac host at 192.168.5.2), SCARAB_PUBLIC_URL=<tunnel URL>, SCARAB_UI_DIR=ui/scarab-web-ui/dist (npm run build first), SCARAB_DEV_INSECURE=1 for now (OAuth login is a later pass); (4) store the App PEM via PUT /v1/secrets at org "_forge", name "github-app" (the reserved connection-credential scope); (5) re-deliver the App's "installation" webhook from the GitHub App settings page (or reinstall) so the connection auto-registers — verify GET /v1/repos lists thulasi-ram/scarab-ci; (6) add .scarab/ci.yaml on a branch: a clone step + a fast real step (e.g. rust:1-bookworm running cargo check -p scarab-identity) + an artifacts: glob, push the branch and open a draft PR; (7) watch the webhook → run → Pods on colima → logs stream → commit status appear on the PR — use scarab logs/the dashboard; (8) verify and report each formerly-untestable path: private-repo clone with a minted installation token (S2 guard: credential-free .git/config), fork read-only downgrade if a fork PR is available, status deep-link resolving through the tunnel; (9) tear down what you start, keep notes of every rough edge as git-bug tickets labeled dogfood. Never touch kubectl contexts other than colima; never force-push; ask me only when a browser step is genuinely needed.
```

## What this proves (the open HUMAN VERIFY items)

- GitHub App auth end-to-end: installation-token minting for clone (9d4d3b1),
  status posting with deep links (ADR-0046), permissions import (809df57).
- Webhook ingest + replay guard against real GitHub deliveries (0618cd8).
- The canonical clone image against a private-capable token path (af1ad8f).
- Real results egress + artifacts from a genuine CI workload (e6c80f1, f1f0ac7).
- The embedded dashboard rendering real, self-hosted runs (7886ecf).

## Known gaps to expect (file as `dogfood` tickets, don't rabbit-hole)

- Run provenance on rows is id/status/time/tenant only (no sha/message yet).
- OAuth login stays off in the first pass (`SCARAB_DEV_INSECURE=1`); a second
  pass wires a GitHub OAuth App via `SCARAB_OAUTH_*`.
- A cargo build of the full workspace in-Pod will be slow with no cache —
  start with a small crate check; layer caching is future work (ADR-0018).
