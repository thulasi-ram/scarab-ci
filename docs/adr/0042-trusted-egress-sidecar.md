# 0042. Trusted per-Pod egress sidecar → fence-scoped results API → Postgres

- **Status:** Accepted
- **Date:** 2026-07-14
- **Deciders:** thulasi.ram (architect)
- **Implements:** [0041](0041-named-results-and-interpolation.md) (the k8s capture path its `Executor::results` left as a follow-up)
- **Relates:** [0008](0008-step-contract.md) (the `/scarab/results/*.json` channel), [0038](0038-invoke-and-local-reuse.md) (untrusted-code isolation this must preserve), [0021](0021-double-effect-fencing.md) (fenced, idempotent capture), [0013](0013-history-and-observability.md) (logs are a *separate*, best-effort channel), [0004](0004-execution-topology.md)/[0005](0005-tenancy-and-k8s-only.md) (agentless, control-plane-drives-k8s-API topology)

## Context

[0041](0041-named-results-and-interpolation.md) made a step's named results a durable
Postgres value (`step_runs.results`) that a downstream `${{ outputs.<step>.<name> }}`
resolves at launch. The **local** backend captures them by reading the step's results
files off disk after it exits. The **k8s** backend's `Executor::results` is still empty —
and closing it is not a small wiring detail, because Scarab is **agentless**: the control
plane drives Pods through the k8s API and there is no process co-located with the workload
(unlike Woodpecker/Drone/Actions, whose runner *agent* brokers all step I/O out). So the
control plane can only observe a Pod through the API surface — status and logs — and:

- **The step Pod is untrusted and credential-less.** Third-party code arrives as an
  isolated OCI image ([0038](0038-invoke-and-local-reuse.md)); it must never hold Postgres,
  object-store, or k8s-API credentials, nor a network path to the control plane. So the step
  cannot write its own results anywhere durable.
- **Every credential-free API-surface channel is lossy.** k8s container logs are best-effort
  observability: the kubelet **rotates and evicts** them (a chatty step pushes a marker out
  of the retained window), a `--follow` stream **drops** on API-server/kubelet restart, and
  logs vanish on **Pod deletion / node death**. k8s **Events** and the **termination message**
  are the same class — GC'd, deduplicated, capped, diagnostic-only. And parsing results out
  of shared stdout is an *injection* surface (the class of bug that forced GitHub Actions off
  `::set-output::` — CVE-2020-15228 — onto files).

Results are **data, not observability**: losing one silently fails a dependent. They need a
channel with **acknowledged delivery** and a **non-shared** surface — which no API-surface
path provides. That forces a trusted egress helper co-located with the Pod. The question is
the *smallest* such helper that preserves the agentless control plane and the untrusted-Pod
trust posture.

## Decision

> **Addendum 2026-08-01 (ADR-0061 s3-drain):** the trusted-egress role this ADR established —
> a Scarab-owned, credentialed container beside the untrusted step doing acknowledged egress —
> now also lives in the `scarab-wsfetch` helper binary, whose egress container drains
> `/workspace` to the Data Depot in-Pod (`hold`/`drain` + a Depot-validated DrainRecord).

A **trusted per-Pod egress sidecar** carries results out with acknowledged delivery:

1. **Files in, over a shared volume.** The step container and a sidecar share an `emptyDir`
   mounted at `/scarab/results`. The step writes `<name>.json` files there (the *same*
   authoring surface as the local backend, and the file-not-stdout lesson from
   [0008](0008-step-contract.md) and GitHub's migration). The step needs no credentials and
   no network.

2. **Confirmed write out, by the sidecar.** After the step container terminates, the sidecar
   (a lightweight image — Alpine + the injected `scarab` CLI) reads the results files and
   **POSTs them to a fence-scoped results endpoint** on the control plane, which writes
   `step_runs.results`. It **retries until acknowledged** (at-least-once); the write is keyed
   by the `{run, step, attempt}` fence and therefore idempotent ([0021](0021-double-effect-fencing.md))
   — a re-drive (sidecar restart, control-plane crash) re-writes the same value deterministically.

3. **Credential isolation is the whole point.** Only the **sidecar** holds a short-lived,
   fence-scoped token; the untrusted step holds nothing and has no egress to the control
   plane (enforced by network policy). Untrusted code therefore cannot call the results API,
   cannot forge another step's results, and gains no ambient authority — the trust posture of
   [0038](0038-invoke-and-local-reuse.md) is preserved. The results API authenticates the
   token, checks it matches the `{run, step, attempt}` it carries, and bounds the payload size.

4. **Fail-closed on undelivered results.** If the sidecar exhausts a bounded retry budget
   without an ack (control plane unreachable), the step is failed — a step whose results could
   not be captured is a failure, because a dependent's reference would be unbound (consistent
   with [0041](0041-named-results-and-interpolation.md) §5's fail-fast). Results we *did*
   capture are already durable in Postgres before the step is marked Succeeded.

5. **A general egress mechanism, results-first.** The sidecar + shared-volume + fence-scoped-API
   pattern is not results-specific: the same channel can later carry progress, structured
   annotations, and artifact manifests. **v1 scope is results only**; the shape is chosen so
   those ride it without a new mechanism.

**Lifecycle.** The k8s executor's Pod spec gains the sidecar and the shared volume. The sidecar
must **outlive** the step container (to drain results after it exits) and then **exit 0** so the
Pod reaches a terminal phase — a native sidecar (an `initContainer` with `restartPolicy: Always`,
k8s ≥1.29) with a "step done" signal over the shared volume, or a plain second container with the
same coordination. The executor's `poll` treats the Pod as complete only once the sidecar has
terminated, and `results` reads the already-persisted row (the sidecar, not the executor, did the
egress). Teardown deletes the Pod only after the results row is durable.

## Consequences

- **Scarab's first co-located trusted helper** — deliberately the *smallest* one: per-Pod,
  ephemeral, credential-isolated, and adopted **only** because every agentless API-surface path
  is lossy for data. It is a thin sliver of the runner-agent pattern, not a standing per-node
  agent, so the control plane stays agentless and k8s stays a replaceable substrate.
- **New surface:** a fence-scoped results-ingest endpoint on the control plane + per-step token
  minting; a sidecar image; Pod-spec changes (sidecar container + `emptyDir`); executor
  poll/teardown coordination with the sidecar.
- **Per-Pod overhead:** one extra lightweight container and a small `emptyDir` per step. Accepted
  for guaranteed result delivery.
- **The local backend is unchanged** — it reads results files directly; the `Executor` port hides
  the difference. Backends converge on the *authoring* surface (`/scarab/results/*.json`), diverge
  on egress.
- **Logs and results are now explicitly different channels** by reliability class: logs ride the
  best-effort k8s API tail ([0013](0013-history-and-observability.md)); results ride the acked
  sidecar path. Neither contaminates the other.

## Alternatives considered

- **Markers on the step's stdout log stream** — rejected twice over: k8s logs are best-effort
  (rotation/stream-drop/deletion lose bytes), so a result could vanish; and parsing commands from
  untrusted shared stdout is an injection surface (GitHub's CVE-2020-15228 → move to files). Fine
  for logs, unacceptable for data.
- **k8s Events / container termination message** — rejected: best-effort diagnostic channels,
  GC'd, deduplicated, size-capped. Same lossiness that rules out logs.
- **A k8s object as the carrier (CRD / ConfigMap / annotation)** — rejected: puts durable result
  data in **etcd**, splitting the source of truth away from Postgres and coupling result
  durability to the execution backend's lifecycle. Inverts the "Postgres is the brain, k8s is
  ephemeral" layering.
- **Node-level log driver (Fluent Bit / Vector DaemonSet)** — rejected: reintroduces a node agent
  *and* still does not guarantee delivery for results (it can fall behind rotation). Right tool for
  logs-at-scale, wrong tool for result correctness.
- **Full runner-agent (Woodpecker/Drone model)** — rejected for now: a long-lived per-node broker
  that owns all step I/O is a far larger architectural shift. The per-Pod sidecar buys acked result
  egress with a fraction of the surface and keeps the control plane agentless. Revisit only if many
  more egress needs (live logs, interactive) accumulate.
- **Object-store staging (sidecar → object store → control plane → Postgres)** — viable and reuses
  the workspace-CAS credential path, but adds a hop and a copy, and the sidecar would need
  object-store credentials. A direct fence-scoped write to the store of record (Postgres) is more
  direct. Revisit if results outgrow a JSON POST / JSONB row (they are small by design,
  [0041](0041-named-results-and-interpolation.md)).
- **The step calls the results API directly (no sidecar)** — rejected: that hands the untrusted
  container a control-plane credential and an egress path, breaking [0038](0038-invoke-and-local-reuse.md)'s
  isolation. The sidecar exists precisely to hold the credential outside the untrusted container.
