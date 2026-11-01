# The kind dev stack

`just up` runs Scarab on one local kind cluster, `scarab-dev`, from the real
Helm chart. Postgres, MinIO and Forgejo run in the cluster too. The server and the
Depot run a `scarab-server` binary built on your machine, so a code change
needs a compile and a Pod restart, not an image build. This is the setup
described in the ADR-0077 Amendment (2026-11-01).

```sh
just up          # build, create the cluster if needed, install the release
just demo        # one-step pipeline, polled to success
just demo-chain  # produce -> consume, checks the workspace crossed a Step boundary
just reload      # rebuild and roll the server and Depot onto the new binary
just up dogfood  # the same stack in GitHub App mode, behind a named Cloudflare tunnel
just e2e         # every live suite, against the release in its e2e mode
just logs        # follow the server's log
just down        # delete the cluster and its data
just down keep   # uninstall the release; keep the cluster, Postgres, MinIO and their data
```

The API and the web UI answer on the host at `SCARAB_ADDR` from
`deploy/kind/.env` (`127.0.0.1:8080` in `.env.example`). `just ui` (Vite)
proxies to the same address, so it works against this stack unchanged.

Requires docker, kind, kubectl, Helm 4, cargo, python3 and curl, plus zig and
`cargo-zigbuild` on macOS. npm is optional: without it the server runs API-only.

## How the binary gets into the Pod

1. `cargo zigbuild -p scarab-server --target aarch64-unknown-linux-gnu.2.36`
   (on macOS; `x86_64-...` on an amd64 docker) builds a Linux binary against
   bookworm's glibc, the glibc of the runtime image. It also builds
   `scarab-wsfetch`. On Linux, cargo-zigbuild is used when installed and a
   plain `cargo build` otherwise.
2. The binaries are copied into `deploy/kind/.mount/bin/` (copy, then rename,
   so the directory never holds a half-written file), and the web UI's `dist`
   into `deploy/kind/.mount/ui/`. The UI is rebuilt when its sources are newer
   than its `dist`.
3. The kind node mounts `deploy/kind/.mount` at `/scarab` (`extraMounts`).
   colima shares `$HOME` with its VM, so a path under the worktree reaches the
   node.
4. The chart mounts `/scarab` from the node into the server and Depot Pods at
   `/opt/scarab` (`extraVolumes`, `hostPath`) and runs
   `/opt/scarab/bin/scarab-server` (`command`). The image,
   `scarab-runtime:dev` (`docker/runtime/Dockerfile`), is `debian:bookworm-slim`
   with CA roots and the `scarab` user, and holds no Scarab binary.

A directory is mounted, not the file. Cargo writes a new file on each build,
and a file mount keeps showing the old one.

The release's Pod template carries a SHA-256 of the binary and the staged UI's
`index.html` as an annotation, so a `just up` or `just reload` after a change
rolls both Pods, and one with no change rolls nothing.

Measured on 2026-11-01 (M-series Mac, colima 2 vCPU / 4 GiB): `just up` from no
cluster took 2m07s with a warm `target/`; a rerun with no change 16 s; `just
reload` after a one-line change in `scarab-server` 40 s, 9 s of it compiling.
A cold zigbuild of the server took 2m05s in the spike.

## Step images

Step Pods are created by the server, so their helper images cannot come from
the mount. `just up` builds them locally and loads them into the node:

| Image | Built from | Used as |
|---|---|---|
| `scarab-wsfetch:dev` | `docker/runtime/wsfetch.Dockerfile`: the runtime image plus the zigbuilt `scarab-wsfetch` | the fetch init container and the drain helper (`workspace.fetcherImage`) |
| `scarab-results-sidecar:dev` | `docker/sidecar` | the results sidecar (`scarab.sidecarImage`) |
| `scarab-clone:dev` | `docker/clone` | the clone library's image, and the image `just demo` runs |

Local builds keep these helpers in step with the tree. A published `edge` tag
is built from `main`, and a `scarab-wsfetch` older than the server fails every
drain. The release gets a library manifest whose copy of `libs/clone.yaml`
pins `scarab-clone:dev`.

Images are loaded as a one-platform archive (`docker save --platform` and
`kind load image-archive`). `kind load docker-image` imports every platform
and fails on a multi-arch image under the containerd image store. A load is
skipped when the node already has the same image.

## Postgres, MinIO, Forgejo and modes

`postgres.yaml` and `minio.yaml` run one Postgres (with
`track_commit_timestamp=on`, which `just chaos` needs) and one MinIO, each on a
PVC. `forgejo.yaml` runs one Forgejo (SQLite on a PVC; see "Forgejo" below). A mode is a set of Helm values, and each mode has its own database and
bucket on those two servers:

| Mode | Recipe | Database | Bucket | Auth |
|---|---|---|---|---|
| dev | `just up` | `scarab` | `scarab-logs` | dev-insecure |
| dogfood | `just up dogfood` | `scarab_dogfood` | `scarab-dogfood` | dev-insecure, GitHub App |
| e2e | `just e2e` | `scarab_e2e` | `scarab-e2e` | dev-insecure; Forgejo behind a quick tunnel |

Switching modes deletes the Depot's warm PVC. The warm tier caches blobs of
one bucket, and would otherwise serve the new mode reads its bucket cannot
back. The Depot is a cache (ADR-0066), so it refills from the bucket. A switch
is refused while step Pods (label `scarab.io/run`) of the current mode are
running, since they would carry on against the other mode's server, Depot and
secrets; `SCARAB_KIND_FORCE_SWITCH=1` switches anyway.

Dev secrets (master key, workspace token secret, results token secret) are
generated once into `deploy/kind/.state/` (gitignored, mode 600) and kept
across `just down`, so a kept database still decrypts.

## `just up dogfood`

This repository's own CI on Scarab, through the GitHub App, reached from
GitHub through a named Cloudflare tunnel. It reads
`~/.config/scarab/dogfood.env` key by key and never prints a value:

| Key | Use |
|---|---|
| `SCARAB_GITHUB_APP_ID`, `SCARAB_APP_PEM` | App mode; the PEM is mounted from a Secret |
| `SCARAB_GITHUB_WEBHOOK_SECRET` | `/webhooks/github` signatures |
| `SCARAB_MASTER_KEY`, `SCARAB_RESULTS_TOKEN_SECRET`, `SCARAB_WORKSPACE_TOKEN_SECRET` | release secrets |
| `SCARAB_ORG`, `SCARAB_REPO`, `SCARAB_INSTALL_ID` | the installation registered on a fresh database |
| `SCARAB_STEP_TIMEOUT_SECS` | default step deadline |
| `CLOUDFLARE_TUNNEL_TOKEN`, `DOGFOOD_HOSTNAME` | the tunnel; `just up dogfood` refuses without them |

Each line is `KEY=value`; a leading `export `, CRLF line endings and one pair
of quotes around the value are accepted, and nothing else is interpreted.
`SCARAB_ORG` is required. `DOGFOOD_HOSTNAME` is a bare host name
(`https://` is dropped; a space or a path is refused). Secret values go into
the release as JSON-escaped Helm values, so quotes, backslashes and `#` are
kept as they are.

`cloudflared.yaml` runs cloudflared in token mode, so the route lives in the
Cloudflare Zero Trust dashboard: the tunnel's public hostname must point at
`http://scarab.scarab.svc.cluster.local:80` with Path `^/webhooks/`, so that
only GitHub's deliveries reach Scarab and every other path gets the tunnel's
catch-all 404. `publicUrl` is
`https://$DOGFOOD_HOSTNAME`. After the roll-out, `up.sh` seeds the
`production` Environment, installs the clone library into `SCARAB_ORG` and
registers the installation with a signed `installation:created`, so a fresh
database knows the installation before GitHub sends anything.

The dogfood runs dev-insecure, as it did on colima: there is no login, and
anyone who reaches the UI or the API is an Owner of a server that holds the
GitHub App's key. That is why the hostname carries only `/webhooks/`. The UI
and API stay on the host at `SCARAB_ADDR` (`http://127.0.0.1:<port>`).
Webhook signatures are checked with or without dev-insecure.

Once cloudflared has an edge connection, `up.sh` checks this from outside,
through Cloudflare: `GET /` and `GET /v1/repos` on `https://$DOGFOOD_HOSTNAME`
must both answer 404. When the local resolver has no answer for the name (a
cached "no such name" from before the DNS record existed), it resolves the
name through 1.1.1.1 with `dig` and passes `--resolve` to curl. Any other
answer, or none within a minute, scales cloudflared to 0 and fails
`just up dogfood` with a message naming the dashboard setting. The tunnel is
public for those few seconds. It then sends an unsigned
`POST /webhooks/github` and expects Scarab's 400 or 401.

`just up` switches back to dev and removes the tunnel.

### GitHub App settings

The webhook URL and secret are not enough. Two App settings must be right, and
when either is wrong nothing reports an error:

1. **Subscribe to events.** Under *Permissions & events > Subscribe to events*,
   check **Push** and **Pull request**. With an empty list, `push` and
   `pull_request` are never delivered, so nothing triggers. `installation` and
   `installation_repositories` still arrive (they do not depend on the list),
   so the repo registers and `GET /v1/repos` looks fine: the repo is
   registered, but nothing ever runs.
2. **Grant Commit statuses: read & write.** Without it, posting a status gets
   403 `Resource not accessible by integration`. Scarab logs it and
   dead-letters the status message after retries, but the Run still succeeds,
   so the PR never shows a status. Also grant **Contents: read** (clone) and
   **Metadata: read**.

After a permission change, GitHub asks an installation owner to approve the
new access by email; the change is not live until then.

## Kube safety

Every kubectl and helm call uses `deploy/kind/.kubeconfig` and checks first
that its server is the API port docker publishes for `scarab-dev-control-plane`
(`lib.sh`). The ambient `~/.kube/config` is never read.
`deploy/kind/kubectl.sh <args>` runs kubectl the same way.

A `scarab-dev` cluster without this worktree's mount or the host port (one
from another worktree, or one from before `SCARAB_ADDR` changed) is refused with the reason, since recreating it deletes
its Postgres and MinIO data. `SCARAB_KIND_RECREATE=1 just up` recreates it.
The host port is checked first, so a port held by something else stops
`just up` before anything is deleted.

`SCARAB_ADDR` is read by one helper (`scarab_addr` in `lib.sh`) for `up.sh`
and `just demo`: a trailing comment, quotes and CRLF are dropped, `localhost`
or an empty host means `127.0.0.1`, a missing port means `8080`.

## Tests on the release

`just e2e` (`e2e.sh`) brings the release up in its e2e mode and leaves it
there; `just up` switches it back to dev. It runs every live suite in passes,
and all passes run even when one fails:

| Pass | What |
|---|---|
| e2e | `crates/scarab-e2e`: happy path, cancel, rerun Takes |
| forgejo-adapter | `scarab-forge-forgejo` `tests/live.rs` against the real Forgejo: pick-list pagination, hook signing, the push payload |
| forgejo-onboarding | add connection → pick-list → bind → hook → real push → Run, through the server; one test at a time |
| forgejo-tunnel | the forge_token cases, through the quick tunnel |
| cluster | the executor's and the plugin runtime's live cluster suites, in namespace `scarab-kube-tests` |
| crash_resume | last, on its own: it kills the server every other case talks to |

`just e2e <filter>` passes a nextest filter to every pass. The test binaries
run on the host. `test-env.sh` gives them what the host cannot reach directly:

- Postgres (for the e2e case that seeds a connection), Forgejo, the Depot and
  MinIO (for the cluster suites, whose test process stands in for the control
  plane) through `kubectl port-forward` on a free loopback port;
- the host address a Pod reaches the host at, for a test that is itself a
  server the cluster calls (`happy_path`'s fake forge, the Forgejo adapter's
  capture listener, the results listener of the executor suite). A listener on
  the host is asked for from a busybox Pod; the first of
  `host.docker.internal`, the kind network's gateway and `host.lima.internal`
  that answers wins. On macOS the gateway is the VM's, not the host's.

`crash_resume` kills the server with SIGKILL from the kind node's PID
namespace (`crictl inspect` gives the container's PID on the node), so the
kubelet restarts the container in the same Pod.

## Forgejo

Forgejo runs in every mode, like Postgres and MinIO, so the Forgejo adapter is
checked against a real Forgejo (`data.forgejo.org/forgejo/forgejo:16.0.1`) and not
only against the shapes we believed it sends (git-bug 3863d5e: a hook created
with no `config.secret` shipped once). The server and step Pods reach it at
`http://scarab-forgejo.scarab.svc.cluster.local:3000`; the tests through a
port-forward. Two settings in `forgejo.yaml` are required:
`webhook.ALLOWED_HOST_LIST=*` (by default Forgejo refuses to deliver to
private addresses, and the tier would pass without testing ingest) and
`security.INSTALL_LOCK=true`.

`forgejo.sh` seeds it on every `up.sh`: an admin user (`scarab-admin`, a random
password in `.state/forgejo-admin-password`), a fresh API token, the working
repos with a `.scarab/ci.yaml`, 55 filler repos (more than the adapter's page
size of 50, so the pick-list must page), an org repo reached by a team member
and a collaborator, and the private repos of the forge_token cases. It writes
`.state/forgejo.env` (mode 600), the `SCARAB_TEST_FORGEJO_*` values the tests
read. Its hooks are stamped with `.state/forgejo-webhook-secret`, which the
release gets as `secrets.forgejoWebhookSecret`.

In e2e mode `up.sh` first starts a cloudflared quick tunnel in front of it
(`forgejo-tunnel.yaml`), reads the random `https://<name>.trycloudflare.com`
URL from its log, and sets Forgejo's `ROOT_URL` to it with sign-in required to
view. The forge_token cases need an https clone URL that the server Pod and the
step Pods both reach, over TLS the clone image trusts; a quick tunnel needs no
account. This makes network calls, and Forgejo is reachable from the internet
while the tunnel runs; `just e2e` deletes the tunnel when it ends, and the next
run gets a new URL. In e2e mode the release's library manifest is the real
`libs/` at its last commit, which the server fetches from github.com, so
`libs/` must be committed and pushed; the release's `publicUrl` is the server's
Service, where Forgejo delivers hooks.

## CI

`.github/workflows/e2e.yml` runs `just e2e` on PRs labelled `ci:full` and
weekly, so CI creates the same cluster with the same `up.sh`. A PR without the
label runs `ci.yml` only (the workspace suite, no cluster). The runner gets
pinned kind, kubectl, Helm 4 and zig from `.github/actions/kind-tools`, and
`cargo-zigbuild`. With `cargo-zigbuild` installed, `up.sh` builds for
`x86_64-unknown-linux-gnu.2.36` there too: a plain `cargo build` on
ubuntu-latest links a newer glibc than the runtime image's (bookworm, 2.36).
On a Linux runner the kind network's gateway is a host address, so the
Pod-side probe in `test-env.sh` finds it.

## Settings

| Variable | Default | Effect |
|---|---|---|
| `SCARAB_KIND_PROFILE` | `dev` | cargo profile for the binaries (`release` for a faster drain, slower build) |
| `SCARAB_KIND_SKIP_BUILD` | `0` | `1` uses the binaries already in `.mount/bin` |
| `SCARAB_KIND_SKIP_UI` | `0` | `1` builds no web UI and stages none; the server runs API-only (CI sets it) |
| `SCARAB_KIND_FORCE_SWITCH` | `0` | `1` switches dev <-> dogfood while step Pods are running |
| `SCARAB_KIND_RECREATE` | `0` | `1` deletes and recreates a `scarab-dev` cluster that does not fit (its data goes with it) |
| `SCARAB_DEV_ORG` | `thulasi-ram` | org whose `production` Environment dev mode seeds |
| `SCARAB_DOGFOOD_ENV` | `~/.config/scarab/dogfood.env` | the dogfood env file |
| `SCARAB_DOGFOOD_DEV_INSECURE` | `true` | dogfood's `scarab.devInsecure` |
| `SCARAB_KIND_EXTRA_VALUES` | none | one more Helm values file, applied last; not remembered by `just reload` |
