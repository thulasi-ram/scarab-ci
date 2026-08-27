# Handoff — the public demo (`demo-scarab.ahiravan.dev`) on Oracle + k3s

Stands up a **publicly reachable Scarab demo for $0/month**: one Oracle Cloud
Always Free ARM box running k3s, the control plane, the workspace service and
every step Pod, reached through a Cloudflare Tunnel, with Cloudflare R2 as the
object store. The deployment mode lives in `deploy/demo-oracle/` (new; see its
README, which is the operator guide).

Not merged, not deployed yet — **one blocking bug** stands between the current
state and a first deploy. See "Blocked on" below.

## Why this shape (decisions, so they are not relitigated)

- **Oracle Always Free**, not Hetzner (~€16/mo) or managed k8s ($25-45/mo). The
  accepted costs are capacity scarcity at create time and Oracle's right to
  reclaim an idle instance.
- **Cloudflare Tunnel, not an ingress controller.** No inbound ports, no public
  listener, no LoadBalancer, no cert-manager — cloudflared dials OUT. This is
  also why the OCI security list stays SSH-only. Traefik and servicelb are
  disabled in k3s for the same reason.
- **R2, not in-cluster MinIO.** The instance can be reclaimed; state that lives
  only on the box is state you lose. Free tier (10 GB, 1M Class A) covers a demo,
  and egress is free.
- **In-cluster Postgres, not Neon.** Neon's free tier is 100 CU-hours/month and
  the server holds a pool with a scheduler tick, so it never idles: 0.25 CU x
  730h ~= 182 CU-h. It would run out mid-month.
- **Ubuntu, not Oracle Linux.** OL9 runs SELinux enforcing (k3s then needs the
  `k3s-selinux` policy or containers fail in ways that never name SELinux) and
  uses firewalld rather than the plain iptables path `bootstrap.sh` repairs.
  CentOS is worse still: CentOS Linux 7 is EOL, Stream is a moving target, and
  both carry the same tax with none of OL's compensating Ksplice/OCI integration.
- **Login-gated, real OAuth.** `DEV_INSECURE` is refused by `deploy.sh` for this
  mode. Visitors sign in with GitHub and land as **Viewer**, which has no Write,
  so no visitor can dispatch a run. There is **no anonymous read path** in the
  server (checked) — a public read-only principal would be a server feature and
  an ADR, not config.

## What's already true (don't redo)

**The host.** `ssh demo-scarab` (user `ubuntu`). Oracle Cloud, region
`ap-mumbai-1` — a **single-AD region**, so there is no "try another AD" lever.
`VM.Standard.A1.Flex`, **2 OCPU / 12 GB**, aarch64, Ubuntu 24.04.4 LTS, 200 GB
boot volume (the whole Always Free storage allowance, deliberately — no second
volume to manage).

**k3s v1.36.3+k3s1 is up and verified.** One Ready node. traefik, servicelb and
metrics-server genuinely absent (no manifests, no svclb pods, no metrics
APIService). `local-path` is present and default — load-bearing, since both PVCs
(workspace warm CAS, Postgres) depend on it. Pod networking was smoke-tested for
real: cluster DNS, service-CIDR routing, and external egress to `api.github.com`
and `ghcr.io`, at 5ms DNS latency rather than the ~5s timeout that signals the
unrepaired REJECT.

**Oracle's stock iptables is repaired.** Their Ubuntu image ends both INPUT and
FORWARD with `-j REJECT --reject-with icmp-host-prohibited`; the FORWARD one is
what silently breaks k3s pod networking. `bootstrap.sh` inserts ACCEPTs for
`10.42.0.0/16` and `10.43.0.0/16` at position 1 in both chains and persists them.
Oracle's own `InstanceServices` chain (17 rules, OCI metadata/iSCSI) is intact.

**Credentials are in place and four of five are independently validated:**

| What | Verified how |
|---|---|
| R2 | list + put + get + delete, run from a Pod **inside the cluster** (the real network path) |
| Cloudflare Tunnel | cloudflared registered over QUIC at `bom03`; Cloudflare pushed back its ingress config, confirming `demo-scarab.ahiravan.dev -> http://scarab.scarab.svc.cluster.local:80` |
| GitHub App | `GET /app` returns slug `demoscarab-githubapp` (so the PEM matches the App ID); the App is installed on `thulasi-ram/scarab-ci` and reports exactly the 7 permissions below |
| Master key | 44 chars base64, decodes to exactly 32 bytes |
| OAuth client secret | **NOT validated** — only a real browser login can prove it |

App permissions, derived from the endpoints `scarab-forge-github` actually calls
(not guessed): `contents:read`, `metadata:read`, `statuses:write`,
`deployments:write`, `issues:write`, `pull_requests:write`,
`administration:read` (the last for `collaborators/{user}/permission`, the
ADR-0049 role import). Subscribed events: `push`, `pull_request`, `release`,
`issue_comment` — exactly the four the adapter normalizes (`lib.rs:53-134`).

**Secrets on the box** (never in git, never in a transcript): `.env` at
`~/scarab/deploy/demo-oracle/.env` (0600); App PEM at
`/home/ubuntu/secrets/scarab-demo.private-key.pem` (0600). The master key,
webhook secret and results-token secret were generated **on the box** with
`openssl`. `SCARAB_MASTER_KEY` must stay stable forever — if the box is rebuilt,
copy it off first or every stored secret becomes unreadable.

## Host access (the SSH key is on one laptop only)

```
Host demo-scarab
  HostName 80.225.244.142      # ephemeral public IP: survives stop/start,
  User ubuntu                  # released only on TERMINATE
  IdentityFile ~/.ssh/demo-scarab
```

The keypair is **dedicated to this box** (`ssh-keygen -t ed25519 -f
~/.ssh/demo-scarab`), generated locally so the private half never travelled
through a browser download. It currently exists on **one machine**. Anyone else
picking this up needs it copied to them, or has to add their own public key
through the OCI console / serial Instance Console Connection. Back it up
alongside `SCARAB_MASTER_KEY`.

Gotcha worth recording: the instance was created **without** a public IP (the
"Assign a public IPv4 address" checkbox in the create flow). Fixed after the fact
via Instance -> Attached VNICs -> primary VNIC -> IPv4 Addresses -> the private
IP row -> Edit -> **Ephemeral Public IP**. That only works because the VNIC is in
a **public** subnet; a VNIC in a private subnet cannot be given one and cannot be
moved, so the recovery there is terminate-and-recreate (and a fresh roll of the
capacity dice).

The public IP exists **only for SSH**. The tunnel is egress-only. The OCI
security list allows tcp/22 and nothing else, and it should stay that way — if
you find yourself opening 80 or 443 to make the demo reachable, the tunnel is
misconfigured.

**Installed on the box** (by `bootstrap.sh` or alongside it): k3s
`v1.36.3+k3s1` (its bundled `kubectl`), `helm v3.21.4`, `git 2.43.0`. **`just` is
NOT installed** — on the box, run `bash deploy/demo-oracle/deploy.sh` directly;
the `just demo-oracle` recipe is for a workstation. Repo clone lives at
`/home/ubuntu/scarab`.

## External configuration (recreate this if the box dies)

None of it lives in the repo, and all of it is required.

**Oracle.** Region `ap-mumbai-1` (home region, **not changeable**). Shape
`VM.Standard.A1.Flex` 2 OCPU / 12 GB, image Canonical Ubuntu 24.04 **aarch64**,
boot volume **200 GB** with Balanced (VPU 10) performance, public subnet with a
public IPv4. Leave the **Oracle Cloud Agent's Compute Instance Monitoring plugin
ENABLED** — it is what reports CPU/memory/network to OCI Monitoring, and those
are the metrics the idle-reclamation policy is evaluated against. Boot volume
encryption stays Oracle-managed; a customer-managed Vault key adds a way to lose
the volume permanently and buys nothing here.

If a recreate hits `Out of host capacity` — the normal experience in Mumbai, and
there is no second AD to try — the levers are, in order: retry on a loop (capacity
frees in bursts), leave the fault domain unpinned, request a smaller shape
(1 OCPU / 6 GB) as a foothold and resize in place later, or **upgrade the tenancy
to Pay As You Go**, which keeps all Always Free resources free, materially
improves A1 availability, and removes idle reclamation — at the cost of real
billing liability, so pair it with a $1 budget alert.

**Cloudflare.** Domain `ahiravan.dev` registered at Cloudflare Registrar, so the
zone was active immediately with no nameserver change. Named tunnel `scarab-demo`
with one public hostname: `demo-scarab.ahiravan.dev` -> **HTTP** ->
`scarab.scarab.svc.cluster.local:80`. Saving that hostname creates the proxied
DNS record itself — never add the CNAME by hand. R2 bucket **`demo-scarab`**
(note: `.env.example` still defaults to `scarab-demo`), with an **Abort multipart
uploads -> 1 day** lifecycle rule, and an R2 API token scoped to that bucket with
**Object Read & Write**. The S3 credential is the token's **Access Key ID /
Secret Access Key** pair, never the "token value" shown beside them.

**GitHub — two separate apps, which is the easy thing to get wrong.**

- *OAuth App* (login only): homepage `https://demo-scarab.ahiravan.dev`,
  **callback `https://demo-scarab.ahiravan.dev/v1/auth/callback`** exactly.
  "Expire user access tokens" is safe to leave **enabled**: `oauth.rs:243` reads
  `access_token` once, immediately, to call `userinfoUrl`, and there is no
  `refresh_token` handling anywhere — Scarab then mints its own 24h Postgres
  session. GitHub's token lifetime is measured in seconds regardless.
- *GitHub App* `demoscarab-githubapp` (webhooks, statuses, clone credential):
  **webhook URL `https://demo-scarab.ahiravan.dev/webhooks/github`**, webhook
  secret = `SCARAB_GITHUB_WEBHOOK_SECRET` from `.env`. Its **callback URL is not
  needed** (leave blank; do not tick "Request user authorization (OAuth) during
  installation") — user login goes through the OAuth App. Permissions and
  subscribed events are listed above. Install it on `thulasi-ram/scarab-ci`.

## Blocked on — GHCR package ownership after the repo was recreated

`thulasi-ram/scarab-ci` was **created 2026-08-27T18:38:21Z**, and the old name
does not redirect (404), so this was a recreate rather than a rename — a new
repository **ID** behind the same name. The four GHCR packages predate it
(`scarab-server:edge` was built 2026-08-26 by the previous repo) and are
user-scoped, so this repo is not on their Actions-access list.

Every build job now runs and fails identically at the push:

```
#11 ERROR: failed to push ghcr.io/thulasi-ram/scarab-results-sidecar:
    denied: permission_denied: write_package
```

Note "Log in to GHCR" **succeeds** in every job — this is authorization on the
package, not authentication, and not the workflow: `image.yml` already declares
`packages: write` at job level (lines 54-56, 132-134), which overrides the
repo's read-only `default_workflow_permissions`.

The same orphaning explains the wsfetch anomaly: all four packages belong to the
old repo; three are public and pull fine, `scarab-wsfetch` is private, so the
node gets `401 Unauthorized` on the anonymous token and every workspace Step Pod
would sit in `ImagePullBackOff`. `deploy.sh` preflights it, so the deploy
refuses instead of half-succeeding.

**Fix, per package** at
`github.com/users/thulasi-ram/packages/container/<name>/settings`:
Manage Actions access -> Add repository -> `scarab-ci` -> **Write**. For
`scarab-wsfetch` also set visibility to **Public**. Then
`gh run rerun <id> --failed`. Needs a token with `admin:packages`, so it is a UI
action.

Deleting the four packages and letting the workflow recreate them (auto-linked
to this repo) also works and is less fiddly, but throws away the only currently
working images — `scarab-server:edge` is the fallback if a rebuild hits
something unrelated. Prefer granting access.

**Earlier and now resolved:** run `33104586966` (18:39:40Z, one minute after the
repo was created) failed in 12s with all 8 jobs refused before starting —
"recent account payments have failed or your spending limit needs to be
increased". Jobs start and build now, so billing is no longer the blocker. Two
separate problems in sequence; do not confuse the two if this recurs.

## Fixed in this session

- **`deploy.sh`'s `.env` loader took inline comments as part of the value.**
  `.env.example` ships `IMAGE_TAG=edge   # or a published sha-<gitsha>`, and the
  loader (a deliberate first-`=` split so base64 values ending in `=` survive)
  passed the whole remainder through, so the image preflight received
  `...:edge   # or a published sha-<gitsha>` and died with `invalid reference
  format`. This was the first-deploy failure. Now strips a trailing `#` comment
  only when whitespace precedes it — `source`'s own rule, which is why sourcing
  the file by hand never reproduced it. Unit-tested against inline comments,
  base64 padding, a `#` inside a value, tab-preceded comments, and URLs.

## Do next (in order)

1. **Settle the GitHub Actions billing**, then confirm `scarab-wsfetch:edge` is
   pullable anonymously:
   ```
   kubectl run wsprobe --restart=Never --image=ghcr.io/thulasi-ram/scarab-wsfetch:edge --command -- /bin/true
   ```
   If the package merely needed its visibility flipped, this passes without any
   rebuild at all.
2. **Re-run the deploy** on the box: `bash deploy/demo-oracle/deploy.sh`. The
   loader bug that stopped the first attempt is fixed; the image preflight is
   the next gate it has to clear.
3. **Prove a real login** — the only unvalidated credential. Sign in at
   `https://demo-scarab.ahiravan.dev`, confirm you land as Owner
   (`SCARAB_OAUTH_OWNERS=thulasi-ram`) and that a second account lands as Viewer
   and **cannot** dispatch.
4. **Prove a real GitHub-delivered run**: push to the repo, watch the webhook
   land, the run go green, and a commit status post back with a resolvable
   `{publicUrl}/runs/{id}` deep link.
5. **Measure what a run actually costs** on 2 cores before deciding anything
   about resizing (see "Sizing" below).
6. Install `just` on the box if the recipe is wanted; today only
   `bash deploy/demo-oracle/deploy.sh` works there.
7. Re-clone the repo on the box now that the mode is committed — it is currently
   running off an `scp`'d working tree, which is fine but is not what the next
   person will reproduce.

## Sizing — deliberately left at 2 OCPU / 12 GB

RAM is not the constraint: ~2 GB control plane leaves ~9 GB for step Pods, and
the heaviest thing `dogfood.yaml` does is three matrix legs plus a BuildKit build
requesting 2 GB. **Cores** are the constraint — Rust compiles are CPU-bound and
there is no layer caching (ADR-0018), which is why `dogfood.yaml` carries
`budget: 7200`.

Two facts make deferring the right call: **an A1.Flex resize is in-place and
non-destructive** (Edit shape -> reboot; if Mumbai has no room it errors and you
keep what you have — no capacity re-roll), and **4 OCPU / 24 GB would consume
2,920 of the 3,000 monthly OCPU-hours**, leaving no headroom for a second
instance without being billed. So: run at 2/12, put the small `.scarab/ci.yaml`
behind the keepalive rather than the kitchen-sink pipeline, and resize later with
real wall-clock numbers instead of speculation.

Unresolved: this tenancy's console advertises 3,000 OCPU-hours / 18,000 GB-hours
(= 4 OCPU / 24 GB continuous), which contradicts widely-reported June 2026
halving to 2/12. Check **Limits, Quotas and Usage -> Compute -> "Cores for Ampere
A1"** for the authoritative per-tenancy number before relying on either. A $1
budget alert is the real safety net.

## Known gaps

- **There is no machine credential for `POST /v1/repos/{org}/{repo}/dispatch`.**
  Traced end to end: `session_id()` (`scarab-server/src/lib.rs:5448`) accepts only
  `Authorization: Bearer <session-id>` or the `scarab_session` cookie; sessions
  are minted in exactly one place, `POST /v1/auth/login`, exchanging a single-use
  OAuth authorization code; TTL 24h; `openapi.json` declares no
  `securitySchemes` at all. No PAT, no service account, no client-credentials
  grant. **Closing this is a server feature** — issued API tokens, a
  `Principal`-bearing credential minted by an Owner with a verb and an expiry —
  and it is worth an ADR.
- **`cron:` triggers are inert.** The DSL parses them and `Event::Cron` exists,
  but nothing constructs one — outside its own unit tests it appears only in
  `match` arms. There is no scheduler.
- Because of both, `demo-keepalive.yml` pokes the demo by **force-pushing an
  empty commit to a `demo-keepalive` branch**, which arrives as a real App
  webhook and produces a real Run. It costs a moving branch in the repo, and it
  doubles as the guard against Oracle's idle reclamation (published thresholds
  are reported inconsistently as 10% or 20% across CPU/network/memory over 7
  days; six real runs a day clears either).
- **Step Pods run in their own `scarab-steps` namespace** with a LimitRange,
  rather than ADR-0055 `default_resources` (which needs
  `SCARAB_PLACEMENT_CONFIG_FILE`, a file-only knob with no chart mount). **No
  other mode in this repo uses a non-default exec namespace.** One-line revert is
  documented in `values.yaml` and `steps.yaml`.
- `helperMemoryMib: "512"` (chart default is unset), accepting the documented
  OOM risk for a single workspace file over ~256 MiB in exchange for the
  scheduler seeing the helper on a small box.
- `cloudflared` is pinned to `:latest`, matching the `minio.yaml` precedent. It
  is the front door; consider digest-pinning.
- Retention TTLs are aggressive for a demo box: logs 7d, artifacts 7d, workspace
  3d, packs 3d, via `SCARAB_RETENTION_*` in `scarab.extraEnv`.
- **Nothing in `deploy/demo-oracle/` has completed a successful deploy.** The
  README carries an explicit `> **Unverified:**` note. R2 end-to-end through
  `object_store` 0.14, and whether two cores carry the pipeline, are both
  untested.

## Fixes already folded back into `bootstrap.sh`

All three were found by running it against the real box; the script now handles
them, and the live box has been repaired to match.

1. **`kubectl` was unusable as `ubuntu`.** `/usr/local/bin/kubectl` is a symlink
   to the k3s binary, and that wrapper prefers `/etc/rancher/k3s/k3s.yaml` over
   `$HOME/.kube/config` and does **not** fall back when it cannot read it — so a
   root-only kubeconfig broke kubectl for the very user `deploy.sh` runs as,
   despite a good copy in their home. Fixed with `--write-kubeconfig-group`
   (an **install-time** flag, hence resolved before the install; on the live box
   it needed a systemd unit edit + restart). Rejected `--write-kubeconfig-mode
   0644`: world-readable cluster-admin credentials on a public box.
2. **Re-running the script poisoned `/etc/iptables/rules.v4`.**
   `netfilter-persistent save` snapshots the *live* ruleset, so running it after
   k3s was up baked in 79 lines of kube-proxy/kube-router/flannel runtime chains
   — including stale `kube-dns ... has no endpoints -j REJECT` rules that are
   restored **at boot, before k3s starts**. Self-healing via kube-proxy's full
   sync, but it is exactly the failure class that section exists to prevent. The
   save is now skipped when k3s is already installed.
3. **The final `kubectl wait` raced.** The installer returns when systemd reports
   the unit active, which is before the node object exists, and `kubectl wait
   --all` does not wait on an empty set — it exits 1 with "no matching resources
   found". Now polls for the object first.

Also added: `net.core.rmem_max/wmem_max = 7500000`. cloudflared carries this
demo over QUIC and quic-go wants a ~7 MiB receive buffer; Ubuntu's 208 KiB
default caps tunnel throughput, which here is the ceiling on streaming step logs
to the UI.
