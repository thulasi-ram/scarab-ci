# Handoff — dogfood on Helm (in-cluster), next steps

Continues `docs/handoffs/dogfood-scarab-on-scarab.md`. Assumes **PR #22 and
#23 are merged to `main`**. The dogfood loop already runs **green in-cluster**
(webhook → run → clone → `cargo check` → `pending`→`success` commit status with
a run deep-link) on colima via Helm. This session's job is to run it off the
**real multi-arch GHA image** and a **stable tunnel**, then clear the remaining
`dogfood` tickets.

## What's already true (don't redo)
- App is fully configured: `statuses:write` + `checks` write, events subscribed
  (`push`, `pull_request`, `status`). Verified: real `scarab` commit statuses post.
- In-cluster Helm stack works: `deploy/local/` (README there is the operator guide).
  - `deploy.sh` (colima-context-guarded) → in-cluster Postgres (PVC) + `helm upgrade`.
  - `reseed.sh` → stores the App PEM + re-registers the installation via a signed
    synthetic webhook (no browser). Only needed on a **fresh/wiped** DB.
  - All config in the **gitignored** `deploy/local/.env` (copy from `.env.example`).
- Chart: App-mode knobs are first-class values; a writable `/tmp` emptyDir is
  mounted (PR #22) so the post-step settle doesn't EROFS-dead-letter.
- Multi-arch images: `image.yml` builds amd64+arm64 on native runners and merges
  a manifest list, so an arm64 colima can pull the same tag CI publishes.

## Runtime state at handoff (likely STALE — reverify)
- `kubectl` context MUST be `colima` (never the EKS contexts). `deploy.sh` enforces this.
- A `scarab` release + `scarab-postgres` were running in namespace `scarab`,
  exposed via `kubectl port-forward -n scarab svc/scarab 8899:80`.
- The server was running the **local** `scarab-server:dogfood-local` image (built
  WITHOUT the #23 clone fix). The **cloudflare quick-tunnel is DOWN** and its URL
  churns on restart — this is the main external gap.
- All runs so far used a **synthetic (hand-signed) push** to `localhost:8899`
  because the tunnel was down; everything downstream (clone, cargo, status) was real.

## Do next (in order)
1. **Deploy the real GHA image by SHA.** ghcr packages are public now.
   - Find the built tag: `gh run list --workflow=image.yml --branch main` → the
     merge commit's image is `ghcr.io/thulasi-ram/scarab-server:sha-<mergesha>`
     (also `:edge`). Confirm it's multi-arch (has an arm64 entry).
   - In `deploy/local/.env`: set `IMAGE_REPOSITORY=ghcr.io/thulasi-ram/scarab-server`
     and `IMAGE_TAG=sha-<mergesha>`. Then `deploy/local/deploy.sh`.
   - This is the real target loop AND validates #23 (clone fix) — check the server
     log shows a minted `contents:read` token clone with **no 422 / no anonymous
     fallback**. Then close git-bug `1b5d5c7`.
2. **Stable tunnel + a real GitHub-delivered run.** Set up a **named** cloudflare
   tunnel (quick tunnels churn). Put its URL in `deploy/local/.env` as
   `SCARAB_PUBLIC_URL` (redeploy) AND in the App's webhook URL (`…/webhooks/github`).
   Then push a branch / open a PR and watch a genuinely GitHub-delivered run (events
   are subscribed) → run → status back on the PR with a **resolvable** deep-link.
3. **Fully prove the private-repo token clone.** `scarab-ci` is public, so the
   minted-token clone can't be fully distinguished from anonymous. Point a small
   **private** test repo's App install at this server (reseed) and confirm the clone
   works only via the token (credential-free `.git/config`, S2 guard).

## Open `dogfood` git-bug tickets (see `git-bug bug` / label `dogfood`)
- `1b5d5c7` clone `contents:read` — **fixed in PR #23**; verify in-cluster (step 1) + close.
- `98ea804` artifacts harvested+uploaded but never DB-indexed (harvest/scheduler
  race) — **real bug, unfixed.** Good `diagnose` candidate.
- `ba921db` `set_status` failures are silent (no log/metric, stuck outbox).
- `c653742` log-tail warn-spam during `PodInitializing`.
- `70aa42e` App-config runbook (events + statuses:write are REQUIRED, fail silently).
- `245a99c` (enhancement) support file/Secret-mounted App PEM (bootstrap-free/GitOps).
- `b04697f` `run_as_root` can't write the CAS workspace (DAC_OVERRIDE dropped).
- Closed: `9cdf38e` (read-only /tmp dead-letter, PR #22), `03ef8e4` (gitignore).

## Gotchas already solved (context, in `.scarab/ci.yaml` + PR #22/#23)
- Steps run non-root baseline; a stock root image (e.g. `rust`) needs
  `security.run_as_root: true`, but the caps-dropped root can't write the
  65532-owned CAS `/workspace` → send `CARGO_TARGET_DIR=/tmp/...` and
  `git config --global --add safe.directory /workspace`.
- Use `bash -c`, not `-lc` (login shell resets PATH, hides rustc/cargo).
- `.env` loader must split on the FIRST `=` (base64 values end in `=`); the file
  is authoritative (never let a stale direnv `.env.local` var win).

## Secrets / identifiers — do NOT commit
The App ID, installation ID, webhook secret, master key, and PEM path live ONLY in
the gitignored `deploy/local/.env` (and the repo's gitignored `.env.local` holds the
stable `SCARAB_MASTER_KEY`). Keep them out of tracked files. NB: an earlier commit
leaked the webhook secret; it was force-push-scrubbed (history clean) — rotating it
is prudent-but-optional. `SCARAB_MASTER_KEY` must stay STABLE or the DB-stored PEM
becomes undecryptable.

## Suggested skills
- **`verify`** — drive the loop end-to-end after deploying the real SHA (don't just
  trust tests): trigger a run, watch pods on colima, confirm the commit status posts.
- **`diagnose`** — for `98ea804` (artifacts indexing race): reproduce, instrument the
  scheduler `poll → settle → executor.artifacts` path, fix, regression-test.
- **`code-review`** — before opening PRs for the remaining ticket fixes.
