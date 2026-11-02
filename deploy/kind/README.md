# The kind dev stack

`just stack up` runs Scarab on one local kind cluster, `scarab-dev`, from the real
Helm chart. Postgres, MinIO and Forgejo run in the cluster too. The server and the
Depot run a `scarab-server` binary built on your machine, so a code change
needs a compile and a Pod restart, not an image build. This is the setup
described in the ADR-0077 Amendment (2026-11-01).

```sh
just stack up          # build, create the cluster if needed, install the release
just stack demo        # one-step pipeline, polled to success
just stack demo chain  # produce -> consume, checks the workspace crossed a Step boundary
just stack reload      # rebuild and roll the server and Depot onto the new binary (after e2e or chaos: dev)
just stack up dogfood  # the same stack in GitHub App mode, behind a named Cloudflare tunnel
just test e2e          # every live suite, against the release in its e2e mode
just test chaos        # ADR-0077 ranks 1, 2 and 4 under faults, then the invariant checker
just stack logs        # follow the server's log
just stack logs depot  # follow the Depot's log
just stack status      # the Depot's readiness and gauges
just stack down        # delete the cluster and its data
just stack down keep   # uninstall the release and both tunnels; keep the cluster, Postgres, MinIO and their data
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
`index.html` as an annotation, so a `just stack up` or `just stack reload` after a change
rolls both Pods, and one with no change rolls nothing.

Measured on 2026-11-01 (M-series Mac, colima 2 vCPU / 4 GiB): `just stack up` from no
cluster took 2m07s with a warm `target/`; a rerun with no change 16 s; `just
stack reload` after a one-line change in `scarab-server` 40 s, 9 s of it compiling.
A cold zigbuild of the server took 2m05s in the spike.

## Step images

Step Pods are created by the server, so their helper images cannot come from
the mount. `just stack up` builds them locally and loads them into the node:

| Image | Built from | Used as |
|---|---|---|
| `scarab-wsfetch:dev` | `docker/runtime/wsfetch.Dockerfile`: the runtime image plus the zigbuilt `scarab-wsfetch` | the fetch init container and the drain helper (`workspace.fetcherImage`) |
| `scarab-results-sidecar:dev` | `docker/sidecar` | the results sidecar (`scarab.sidecarImage`) |
| `scarab-clone:dev` | `docker/clone` | the clone library's image, and the image `just stack demo` runs |

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
`track_commit_timestamp=on`, which `just test chaos` needs) and one MinIO, each on a
PVC. `forgejo.yaml` runs one Forgejo (SQLite on a PVC; see "Forgejo" below). A mode is a set of Helm values, and each mode has its own database and
bucket on those two servers:

| Mode | Recipe | Database | Bucket | Auth |
|---|---|---|---|---|
| dev | `just stack up` | `scarab` | `scarab-logs` | dev-insecure |
| dogfood | `just stack up dogfood` | `scarab_dogfood` | `scarab-dogfood` | GitHub login, GitHub App |
| e2e | `just test e2e` | `scarab_e2e` | `scarab-e2e` | dev-insecure; Forgejo behind a quick tunnel while `just test e2e` runs |
| chaos | `just test chaos` | `scarab_chaos` | `scarab-chaos` | dev-insecure; Postgres and MinIO through toxiproxy; 2 servers, 2 Depot replicas |

Switching modes deletes the Depot's warm PVC. The warm tier caches blobs of
one bucket, and would otherwise serve the new mode reads its bucket cannot
back. The Depot is a cache (ADR-0066), so it refills from the bucket. A switch
is refused while step Pods (label `scarab.io/run`) of the current mode are
running, since they would carry on against the other mode's server, Depot and
secrets; `SCARAB_KIND_FORCE_SWITCH=1` switches anyway.

Dev secrets (master key, workspace token secret, results token secret) are
generated once into `deploy/kind/.state/` (gitignored, mode 600) and kept
across `just stack down`, so a kept database still decrypts.

## `just stack up dogfood`

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
| `CLOUDFLARE_TUNNEL_TOKEN`, `DOGFOOD_HOSTNAME` | the tunnel; `just stack up dogfood` refuses without them |
| `SCARAB_OAUTH_CLIENT_ID`, `SCARAB_OAUTH_CLIENT_SECRET` | GitHub login through the GitHub App's OAuth client; `just stack up dogfood` refuses without them |
| `DOGFOOD_OWNER` | optional; the Owner as `github:<login>`, default `github:$SCARAB_ORG` |
| `DOGFOOD_MEMBER` | optional; a second GitHub account as `github:<login>`, granted the member role |

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
`https://$DOGFOOD_HOSTNAME`. After the roll-out, `up.sh` grants the Owner and
the member their roles (see [Login](#login)) and registers the installation
with a signed `installation:created`, so a fresh database knows the
installation before GitHub sends anything.

The UI and API stay on the host. The server holds the GitHub App's key, so
the hostname carries only `/webhooks/`. Webhook signatures are checked on every
delivery.

### Login

The dogfood stack signs people in with GitHub, as a real install does: it has
the identity store (linked accounts), role bindings, API tokens and passkey
step-up, and no dev-insecure. Sign in at `http://localhost:<port>`, the port of
`SCARAB_ADDR` (`scarab.uiUrl`). Use `localhost`, not `127.0.0.1`: browsers
refuse passkeys on an IP address, and a sign-in started on `127.0.0.1` is sent
to `localhost` first. The session cookie is marked `Secure`, which Chrome and
Firefox accept on `http://localhost`; Safari may not, so use Chrome or Firefox.
The login callback is on `localhost` too, so nothing but `/webhooks/` needs to
be public.

Set it up once:

1. In the GitHub App's settings (the App has its own OAuth client), set the
   **Callback URL** to `http://localhost:8080/v1/auth/callback` (use your
   `SCARAB_ADDR` port), generate a **client secret**, and put the App's
   **Client ID** and that secret in `dogfood.env` as `SCARAB_OAUTH_CLIENT_ID`
   and `SCARAB_OAUTH_CLIENT_SECRET`. `up.sh` puts the secret in the Secret
   `scarab-oauth-github` through a temporary file, so it never shows in `ps`
   or the output.
2. Optionally set `DOGFOOD_MEMBER=github:<login>` to a second GitHub account,
   and `DOGFOOD_OWNER` when the Owner is not `github:$SCARAB_ORG`.

The login provider's id is `github`, so people are `github:<login>`. The Owner
is an Owner at every sign-in (`scarab.oauth.owners`). After the roll-out,
`up.sh` runs `deploy/kind/seed.sh`, which grants the Owner the owner role and
the member the member role in `SCARAB_ORG` with
`scarab-server admin grant --if-absent`, run in the server Pod against its
database, so both appear on the org's Members page. `--if-absent` grants only
to someone who has no role binding in the org, a revoked one included: when
the Owner changes or removes a role in the UI, the next `up` or `reload`
leaves it as the Owner set it. A grant needs someone who has signed in once.
Until then the script prints `sign in as github:<login> at
http://localhost:<port>, then run just stack seed dogfood`; that recipe runs the
grants again.

`SCARAB_ADDR` must be on `127.0.0.1` in dogfood mode; `up.sh` refuses another
address. The UI URL is `http://localhost:<port>`, and the UI and API are for
this machine only.

`just stack demo` refuses dogfood mode: it calls the API without a credential.

On a fresh database, `up.sh` also prints a checklist of the steps that now need
a passkey, which it used to do with plain curl (`just stack seed dogfood
checklist` prints it again). On an existing database it compares the clone
library installed in the org with `deploy/kind/.state/dogfood-clone.yaml`
(read with `psql` in the Postgres Pod) and prints step 3 when they differ:

1. Sign in at `http://localhost:<port>` as the Owner and enroll a passkey in
   Settings > Security. Every new database needs a new one.
2. Org settings > Environments: create `production` and make it eligible for
   all projects; the dogfood pipeline deploys to it.
3. Org settings > Libraries: the clone library must pin `scarab-clone:dev`.
   When it is missing, choose **Paste the files** and paste
   `deploy/kind/.state/dogfood-clone.yaml`. When it pins another image,
   uninstall it first.
4. To call the API, issue a token in Org settings > API tokens.

To check that the member is refused an admin action, sign in as the member and
open Org settings > Members, or `PUT` a binding with the member's API token:
both answer 403.

Once cloudflared has an edge connection, `up.sh` checks this from outside,
through Cloudflare: `GET /` and `GET /v1/repos` on `https://$DOGFOOD_HOSTNAME`
must both answer 404. When the local resolver has no answer for the name (a
cached "no such name" from before the DNS record existed), it resolves the
name through 1.1.1.1 with `dig` and passes `--resolve` to curl. Any other
answer, or none within a minute, scales cloudflared to 0 and fails
`just stack up dogfood` with a message naming the dashboard setting. The tunnel is
public for those few seconds. It then sends an unsigned
`POST /webhooks/github` and expects Scarab's 400 or 401.

`just stack up` switches back to dev and removes the tunnel.

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
its Postgres and MinIO data. `SCARAB_KIND_RECREATE=1 just stack up` recreates it.
The host port is checked first, so a port held by something else stops
`just stack up` before anything is deleted.

`SCARAB_ADDR` is read by one helper (`scarab_addr` in `lib.sh`) for `up.sh`
and `just stack demo`: a trailing comment, quotes and CRLF are dropped, `localhost`
or an empty host means `127.0.0.1`, a missing port means `8080`.

## Tests on the release

`just test e2e` (`e2e.sh`) brings the release up in its e2e mode, with plugins
on, and leaves it there; `just stack up` switches it back to dev. It runs every
live suite in passes, and all passes run even when one fails; the exit code is
non-zero when any failed:

| Pass | What |
|---|---|
| e2e | `crates/scarab-e2e`: happy path, cancel, rerun Takes |
| forgejo-adapter | `scarab-forge-forgejo` `tests/live.rs` against the real Forgejo: pick-list pagination, hook signing, the push payload |
| forgejo-onboarding | add connection → pick-list → bind → hook → real push → Run, through the server; one test at a time |
| forgejo-tunnel | the forge_token cases, through the quick tunnel |
| cluster | the executor's and the plugin runtime's live cluster suites, in namespace `scarab-kube-tests` |
| plugin-argocd | `plugin-verify.sh`: builds the Argo CD plugin image and loads it, starts a fake Argo CD on the host, installs and binds the plugin through the API, then checks the isolation probe, the Pod's read of Argo CD through the broker with the bound credential, and that the kill switch makes the broker answer 403 and scales the workload to 0. It stops the fake Argo CD when it ends |
| crash_resume | last, on its own: it kills the server every other case talks to |

`just test e2e <filter>` passes a nextest filter to every nextest pass; the
plugin-argocd pass runs only when the filter is part of its name (so `just test
e2e argocd` runs it alone, with the nextest passes matching nothing). The test binaries
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

## `just test chaos`

`just test chaos` (`chaos.sh`) runs ADR-0077's fault scenarios on this release,
at two server replicas and two Depot replicas:

- rank 1, `mid-drain` and `mid-feed`: one Depot Pod deleted with grace 0 while a
  workspace drain is in flight, then while a fetcher pulls a Step's inputs;
- rank 2, `two-replicas` and `kill-server`: no fault, then the server replica
  that holds a launch claim SIGKILLed while its drain is in flight;
- rank 4, `stop-leader`: the scheduler leader SIGSTOPped past its 30 s lease
  until a peer takes over, then SIGCONTed;
- `pause-node` (not an ADR-0077 rank): the whole kind node `docker pause`d
  while a producer's drain is owed, for longer than the 30 s lease TTL and
  claim window and shorter than the Pod's grace period, then `docker
  unpause`d. This is a host suspend: the kubelet, the egress mid-drain and
  both servers freeze together, and the wall clock jumps on the resume. Every
  Run must still succeed.

Server signals are sent from the kind node, because the server is PID 1 in its
container. The `scarab-chaos` binary (`crates/scarab-chaos`) fires the fault and
then checks the invariants.

```sh
just test chaos             # all six scenarios, seed 1
just test chaos rank2       # one rank
just test chaos mid-feed 7  # one scenario, another seed
just test chaos pause-node  # about 8-10 minutes
just test chaos replay deploy/kind/.chaos/<dir>/scenario.json
```

What it does, in order:

1. Switches the release to chaos mode (`up.sh chaos`). The switch is refused
   while step Pods of the current mode run, and it deletes the Depot's PVCs:
   a warm cache filled from another bucket would serve reads the chaos bucket
   cannot back, and hide the faults this looks for. Chaos mode uses its own
   database and bucket on the same Postgres and MinIO, reached through
   toxiproxy (`toxiproxy.yaml`), with `replicaCount: 2`,
   `workspace.replicaCount: 2` and plugins off.
2. Submits three chained Runs at once. Each producer writes 64 files of 2 MiB
   of random bytes and their SHA-256 list into `/workspace`; each consumer
   inherits the workspace and checks every file against the list.
3. Watches the namespace's Pods (a kube watch, one JSONL line per change)
   until the planned Run's Pod shows the trigger, waits the planned delay, and
   deletes the planned Depot replica with `gracePeriodSeconds: 0`:
   - mid-drain: the producer's `step` container exited 0, its egress helper
     still runs, and the Pod has no `scarab.io/workspace-root` annotation
     (the drain record) yet;
   - mid-feed: the consumer's `scarab-workspace-init` fetcher is running.
4. Waits until every Run is terminal or the quiesce bound runs out: the
   lease TTL (30 s) + the workload's time (300 s) + the drain escalation
   (300 s), counted from the fault.
5. Runs the checker and prints one line per invariant and a `VERDICT` line.
6. Cancels, through the API, every Run the chaos database still holds
   non-terminal (one the quiesce bound gave up on would otherwise resume
   beside the next session's workload; a scenario refuses to start while one
   is left). Then switches the release back to the mode that was active
   before (dev or dogfood; from e2e it goes to dev, since restoring e2e opens
   a new public Forgejo tunnel) and runs `just stack demo` there. When `up.sh chaos`
   refused or the build failed, the release was never switched: nothing is
   deleted or switched, and the recipe only exits.

The seed picks the target Run, the delay (2-10 s after the trigger for a
drain, 0-2 s for a feed) and the replica. Each scenario run writes a
directory under `deploy/kind/.chaos/` (gitignored; `SCARAB_CHAOS_DIR` moves
it):

| File | What |
|---|---|
| `scenario.json` | seed, plan, workload, quiesce bound, Run IDs, the fault with its trigger, its time and where it landed (`mid-drain`, `mid-feed`, `before-window`, `after-window`) |
| `pods.jsonl` | every change to a step Pod or a Depot Pod during the run |
| `verdict.json` | the checker's findings and, per green Attempt, the objects and bytes it read back |
| `release-log.txt` | server and Depot log lines that name a step Pod of the scenario's Runs, the fault's target or the replica it hit; written for every scenario |
| `fetcher-log.txt` | mid-feed only: the target fetcher's own log |
| `cache-log.txt` | the Cache probe's `cache restore` lines, one per cached Step Pod |

Each scenario also runs a Cache probe beside its workload: a seed Run, waited
for before the workload, whose cached Step saves a Cache, and a probe Run,
submitted with the workload, whose cached Step restores it from the Cache's
home Depot replica. A Cache needs a Project, so the probe's pipeline is written
to the Forgejo repo `scarab-admin/chaos-cache`, which is bound (without a
webhook) through a fresh connection in the chaos database and dispatched
through the API. The verdict prints the hits, the misses and the restores from
a home replica. These lines are information only: a Cache miss is correct, so
it never fails or narrows a verdict, and the probe's Runs are not counted by
the invariants. A replay writes the recorded pipeline again with a new salt,
so its seed starts cold too.

A replay reads `scenario.json` and fires the same plan (target Run, delay,
replica) without drawing from the seed again. Timing still varies, so a replay
reproduces the class of the fault (where it landed), not the exact
interleaving.

The checker reads the chaos database and the bucket through port-forwards
straight to Postgres and MinIO, not through toxiproxy, and the kube API. It
does not take the API's or a Step's word for success:

| Id | Invariant (ADR-0077 §2) |
|---|---|
| I1a | at most one Pod UID per Attempt |
| I1b | Attempt N+1's step container starts after N's ended, or after N was journalled finished or superseded |
| I1c | per Step at most one succeeded Attempt, one `AttemptStarted` and one `AttemptFinished` per Attempt, and a Succeeded Step's evidence Attempt is that one |
| T1 | exactly one terminal `RunTransitioned` per Run, matching `runs.status` |
| I3 | every scenario Run terminal within the quiesce bound; none left with `creating = TRUE` |
| I4 | every green Attempt's snapshot closure is in committed packs; every tree and blob range is read from the bucket and hashed against its address; a member in another fence's pack needs a `depot_fence_borrows` edge |
| I8 | after quiesce, no Pod of a scenario Run is pending or running, and no per-Pod workspace Secret outlives its Pod |
| I10 | no uncommitted pack row older than 60 s for a fence that has a drain record; every committed pack's object exists at its recorded size |
| W1 | the workload's outcome (not an ADR-0077 §2 invariant): every scenario Run ends `succeeded`, and every green Attempt has an output snapshot |
| N1 | `pause-node` only: no Attempt finished with a lost-node cause ("workspace snapshot lost", "egress sidecar died"); any other failed Attempt is a warning |
| D1 | `two-replicas` only (git-bug 068cc0b): no two different server replicas drained the same scenario Pod over overlapping time. Each drain's interval comes from the replica's `ws-timing` line with `leg: drain` in `release-log.txt` (end = its timestamp, start = end − `total_ms`); a re-drive by another replica after the first drain ended passes. A drain that failed logs no `ws-timing` line. "drain helper exited 12 with no record posted" means a record POST failed (the Depot's `drain_state_incomplete` 422 at two Depot replicas, for one) and is printed as info |
| C1 | the Cache probe (git-bug 12e2f6b): when the probe's seed Run succeeded and no fault hit the Cache's home Depot replica, every restore of the probe Run is a hit from that home. Otherwise it is printed `not exercised` with the reason, and it has no minimum |

W1 is separate from the ADR's I-numbers because I3 and I4 as written there
accept a dead-lettered Run and a green Attempt with no snapshot. The workload
is built so that every Run succeeds: a Run that ends failed or dead-lettered
(git-bug e140121, a Depot outage that dead-letters Runs) fails W1 with its
Run id and status.

Completed step Pods stay in the namespace on main, so I8 counts a Pod in
phase Succeeded or Failed as torn down. An uncommitted pack row of a fence
with no drain record (an abandoned drain) is reported as a warning: the pack
reclaimer removes it later (git-bug ad79c90). ADR-0077's I2, I5, I6, I7, I9
and I11 are not part of rank 1 and are not checked here.

A scenario whose fault did not test what it names is INCONCLUSIVE, not PASS:
no fault fired (the trigger never held, or every Run ended first), the fault
landed `before-window` or `after-window`, or the trigger condition no longer
held when it fired. So is one where an invariant looked at less than the
workload produces: T1 and I3 one per Run, I1a, I1c and I4 one per Attempt
(two per Run). An invariant that looked at nothing prints `not exercised`, not
`ok`. I1b compares a Step's consecutive Attempts, and a Depot fault causes no
retry, so rank 1 never exercises it; it is printed as such and not counted.

N1 exists because the executor's lost-node verdict is an Infra failure that
the engine retries: the Run can still end `succeeded` on a second Attempt and
pass W1. `pause-node` pauses the node for min(grace − 120 s, 240 s), from the
target Pod's recorded `terminationGracePeriodSeconds`. The producer is a
workspace Pod with a grace of 600 s, so the pause is 240 s, and a scenario
takes about 8-10 minutes. A freeze longer than the grace is not survivable by
design: on the resume the kubelet SIGKILLs the egress sidecar and the Runs
dead-letter (git-bug 070d042, closed as not a bug), so the scenario stays
short of it. A scenario file that planned more than 240 s is refused. Its
trigger and window are `mid-drain`'s, with a delay of 0 to 2 s. It is
INCONCLUSIVE when the pause landed outside the drain window, when docker did
not report the node paused, when it did not outlast the 30 s lease TTL and
claim window, or when it reached the Pod's grace. The
harness unpauses the node on every path it controls (a guard on every
return, and a SIGINT or SIGTERM handler), and `chaos.sh` unpauses a paused
node when it starts and first thing when it exits. If chaos.sh itself is
killed (SIGKILL), `just stack up` and `just stack down` find the node paused
and unpause it, and say so. Its port-forwards are
kept open by a loop that restarts each one when it exits, and that ends once
chaos.sh is gone; after the unpause
the harness ends each (their PIDs are in `deploy/kind/.state/forward-*.pid`),
the loop re-opens it on the same port, and the harness waits until Postgres
and the kube API answer before it goes on. Its Pod watch re-lists when a list
takes over 30 s or the watch is quiet for 270 s.

Before the workload starts, the binary waits for the scenario's topology:
as many ready server Pods and Depot Pods as the scenario file records. A
replay of a 2+2 file on a release at another scale fails there (exit 2).

`just test chaos` exits 0 when every verdict passed, 1 when one failed (a finding:
keep the scenario file and replay it), 2 when a scenario could not run, and 3
when none failed but one was inconclusive (rerun it, or try another seed).
It also samples the kind node's memory every 5 s into
`deploy/kind/.chaos/memory-<time>.log` and prints the peak.

Measured on 2026-11-01 (colima 2 vCPU / 4 GiB): the kind node used 1.46 GiB
at 2+2 before the workload (Postgres, MinIO, Forgejo and toxiproxy included)
and 1.72 to 1.91 GiB at the peak with the step Pods running. One scenario
takes 3 to 5 minutes; `just test chaos` with both and the two mode switches took
about 9. With two server replicas, the drain of one Pod is driven from
both: one replica's drain helper often exits 12 (its record POST failed) and
the other replica's re-drive posts the record, with or without a fault.

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
password in `.state/forgejo-admin-password`), a fresh API token (the earlier
`scarab-kind-*` tokens are revoked first), the working repos with a
`.scarab/ci.yaml` (a seed file whose content changed is updated in place), 55 filler repos (more than the adapter's page
size of 50, so the pick-list must page), an org repo reached by a team member
and a collaborator, and the private repos of the forge_token cases. It writes
`.state/forgejo.env` (mode 600), the `SCARAB_TEST_FORGEJO_*` values the tests
read. Its hooks are stamped with `.state/forgejo-webhook-secret`, which the
release gets as `secrets.forgejoWebhookSecret`.

Under `just test e2e` (which sets `SCARAB_E2E_TUNNEL=1` for `up.sh e2e`), `up.sh`
first re-renders Forgejo with sign-in required to view, then starts a
cloudflared quick tunnel in front of it (`forgejo-tunnel.yaml`), reads the
random `https://<name>.trycloudflare.com` URL from its log, and sets Forgejo's
`ROOT_URL` to it. The forge_token cases need an https clone URL that the
server Pod and the step Pods both reach, over TLS the clone image trusts; a
quick tunnel needs no account. This makes network calls, and Forgejo is
reachable from the internet while the tunnel runs. `just test e2e` deletes the
tunnel when it exits, on success, failure or Ctrl-C, and the next run gets a
new URL. Nothing else opens it: `up.sh e2e` without the flag runs without a
tunnel, every `up.sh` run deletes one a run left, `just stack down keep` deletes it, and `just stack reload` after `just test e2e` goes to
dev. In e2e mode the release's library manifest is the real
`libs/` at its last commit, which the server fetches from github.com, so
`libs/` must be committed and pushed; the release's `publicUrl` is the server's
Service, where Forgejo delivers hooks.

## CI

`.github/workflows/e2e.yml` runs `just test e2e` on PRs labelled `ci:full` and
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
| `SCARAB_KIND_FORCE_SWITCH` | `0` | `1` switches modes while step Pods are running (their Attempts fail on resume) |
| `SCARAB_KIND_RECREATE` | `0` | `1` deletes and recreates a `scarab-dev` cluster that does not fit (its data goes with it) |
| `SCARAB_DEV_ORG` | `thulasi-ram` | org whose `production` Environment dev mode seeds |
| `SCARAB_DOGFOOD_ENV` | `~/.config/scarab/dogfood.env` | the dogfood env file |
| `SCARAB_KIND_EXTRA_VALUES` | none | one more Helm values file, applied last; not remembered by `just stack reload` |
| `SCARAB_CHAOS_DIR` | `deploy/kind/.chaos` | where `just test chaos` writes scenario files, Pod records and verdicts |
