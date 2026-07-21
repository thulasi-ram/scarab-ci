# 0058. Runtime service containers: co-located Sidecar (default) + Run-scoped Shared (opt-in)

- **Status:** Accepted
- **Date:** 2026-07-21
- **Deciders:** thulasi.ram (architect)
- **Amends:** [0008](0008-step-contract.md) (reserved `service` as a built-in *step kind*; a Service is **not** a `needs`-able DAG node — see below)
- **Relates:** [0004](0004-execution-topology.md)/[0005](0005-tenancy-and-k8s-only.md) (Pod-per-Step, k8s-only — why "shared across steps" isn't free), [0042](0042-trusted-egress-sidecar.md) (the native-sidecar machinery the co-located form reuses; per-Pod network isolation), [0021](0021-double-effect-fencing.md) (fencing / double-effect — why the shared form is the author's contract), [0007](0007-data-passing-model.md) (the durable data plane a shared DB must *not* smuggle around), [0039](0039-privileged-images.md) (services run author-supplied images under the restricted baseline + governed grants), [0047](0047-retry-classification-and-attempt-model.md) (retry classification — the startup-vs-mid-run recovery split), [0056](0056-run-takes-and-attempt-grain-evidence.md) (Takes — per-Take service instancing; the machine/human Take boundary), [0027](0027-restart-semantics.md) (cascade on failure), [0050](0050-retention-and-gc.md) (namespace-per-run teardown), [0013](0013-history-and-observability.md) (best-effort log tail)

## Context

CI integration tests need throwaway backing services — a Postgres to run migrations against,
a Redis to exercise a cache path. [0008](0008-step-contract.md) reserved `service` (sidecar)
as a built-in step kind but never designed it. The naïve design is Woodpecker/GitHub-Actions
"service containers": declare `services: [postgres]` and every step reaches it at
`postgres:5432` for the whole pipeline/job. That model is cheap **there** because a Woodpecker
workflow (and a GHA job) is a sequence of steps pinned to **one machine** with a shared network —
a service on that network is trivially reachable by all of them.

Scarab has no such machine. [0004](0004-execution-topology.md)/[0005](0005-tenancy-and-k8s-only.md)
make each **Step its own Pod**, network-isolated by policy ([0042](0042-trusted-egress-sidecar.md)),
and the glossary is explicit that a "job" is a named subgraph — *sugar, not structure*. There is
no shared network for a set of steps to hang a service off. So "a service shared across steps" is
not a feature we forgot; it is a capability we **traded away** for the durability wedge (each Step
is the individually re-executable unit). Offering it at all means either a standalone service Pod
with cluster DNS (cross-Pod networking) or co-scheduling a subgraph into one Pod (surrendering
per-step restart).

Two further tensions shape the design:

- **A service is not a DAG node.** A `gate`/`clone`/`invoke` step has an id, an exit code, and an
  Attempt other steps `need`. A backing container has none of these — it is *infrastructure for a
  Step, not a Step*. Forcing it to masquerade as a node gives it no coherent rerun or status
  semantics. So the [0008](0008-step-contract.md) "service is a step kind" framing is wrong and is
  amended here.
- **A shared, mutable, long-lived DB is unfenced external state.** [0021](0021-double-effect-fencing.md)
  neutralises the double-effect hazard for durable step I/O via fencing tokens and per-Attempt
  evidence isolation. A DB that one step seeds and another reads has *no fence* and is *not durable
  state* in the [0007](0007-data-passing-model.md) sense — exactly the hazard the model contains.

## Decision

Two distinct forms, because they are two distinct topologies with different trust and performance
profiles. Neither is a `needs`-able DAG node.

### 1. Sidecar service — the default

A throwaway container **co-located inside the declaring Step's Pod**, reachable at
`localhost:<port>`, alive only for that one Step's execution. Declared as a field on the Step:

```yaml
steps:
  - id: test
    image: rust:1
    command: cargo test
    services:
      - image: postgres:16
        env: { POSTGRES_PASSWORD: test }
        ready: { tcp: 5432 }     # gates the step's main container start
```

- **Reuses [0042](0042-trusted-egress-sidecar.md)'s native-sidecar machinery** (an
  `initContainer` with `restartPolicy: Always`); no k8s `Service` object, no cross-Pod networking.
- **Fenced by inheritance:** it shares the Step's Attempt identity and teardown — it dies with the
  Pod, is re-created fresh on every Attempt, and cannot outlive or be shared beyond the Step. Two
  Steps that each declare Postgres each get their own. This is the common case (one test Step + a
  throwaway DB), and it carries **none** of the durability/fencing cost below.
- The optional `ready:` probe drives the sidecar's startup probe so the **main container does not
  start until the service is ready** — replacing the `sleep 30s` folklore of Woodpecker.

### 2. Shared service — the opt-in

A **Run-scoped** standalone Pod with a cluster DNS name, reachable by the Steps that opt in.
Declared at pipeline level; Steps opt in with `uses:`:

```yaml
services:                        # pipeline-level: the shared instances
  - name: db
    image: postgres:16
    env: { POSTGRES_PASSWORD: test }
    ready: { tcp: 5432 }

steps:
  - id: migrate
    image: migrate:latest
    uses: [db]                   # opt-in → network path + DNS (db:5432) + readiness gate
    command: migrate up
  - id: test
    image: rust:1
    uses: [db]
    needs: [migrate]
    command: cargo test
```

- **Explicit `uses:` opt-in, not ambient reachability.** Woodpecker/GHA make every step reach the
  service because their steps already share a network; Scarab's Pods are network-isolated, so
  ambient would mean a hole for *every* Pod. `uses:` scopes the network-policy hole to exactly the
  opt-in Pods (least-privilege), tells the scheduler which steps to readiness-gate, and documents
  the dependency. Hostname = the declared `name` (`db:5432`), matching the label-as-hostname
  familiarity of GHA/Woodpecker.
- **Eager birth, Run-scoped teardown.** The service starts at **Run start** (services are run
  setup; the small idle cost is accepted for a clean mental model over lazy birth). It is torn down
  when the Run — that Take — reaches terminal, riding namespace-per-run teardown
  ([0050](0050-retention-and-gc.md)), **not a refcount**. The readiness *gate* still applies only to
  opt-in steps: a step without `uses: [db]` never waits on `db`.
- **A fresh instance per Take.** The instance is keyed `{run, take}`. A **Rerun**
  ([0056](0056-run-takes-and-attempt-grain-evidence.md)) opens a new Take with a **new** instance
  that never sees the prior Take's writes — Takes are independent, so reusing the old DB would
  violate the isolation Takes guarantee.
- **Unfenced external mutable state.** A Shared service is *not* durable state and is *not* fenced.
  Intra-Take idempotency across auto-retries and parallel writers is a **step-author contract**
  ([0021](0021-double-effect-fencing.md)) — exactly the rule for any other external effect
  (`terraform apply`, `docker push`). Scarab provisions the container; it does **not** promise
  exactly-once semantics against it.

### Governance (both forms)

A service image is author-supplied and **no more trusted than a step image**, so it runs under the
same [0039](0039-privileged-images.md) regime: the hardened **restricted** baseline by default;
escalations (`run-as-root`, `add-capabilities`, `privileged`) are governed grants keyed on the
**service image digest** + Environment. Stock DB images (the official `postgres` image *can* start
as root to fix perms) nonetheless run **non-root here without any grant**: the executor pins the
image's built-in service uid and sets `fsGroup` so the ephemeral `emptyDir` is group-writable — the
standard k8s non-root pattern, keeping the service inside the restricted baseline. `run-as-root`
survives only as a **self-service escape hatch** (sandbox-bound) for the rare image that genuinely
cannot run non-root — deliberately *not* the default path, because root-in-container is exactly what
the restricted baseline (and k8s PSS `restricted`, OpenShift's random-uid, rootless runtimes) is
moving away from. No "services are magically trusted" exemption.

### Readiness & recovery

- **Readiness is scheduler-gated.** The author supplies a `ready:` probe (TCP-connect on the first
  port by default; exec/http also allowed). For a Shared service the scheduler holds opt-in steps
  until the probe passes — a natural durable suspend, no `sleep`. Fail-closed: a ready-timeout fails
  the opt-in steps with an unbound-dependency diagnostic.
- **Startup flake → bounded auto-retry.** If a Shared service fails to become ready the *first*
  time, the service Pod is auto-retried within a bounded budget — nothing has been written, so
  nothing is at stake. Exhausted → fail the Run.
- **Mid-run death → fail-closed, no auto-rerun.** If a healthy Shared service dies mid-run, the
  opt-in steps fail and their descendants cascade ([0027](0027-restart-semantics.md)); non-opt-in
  steps proceed. The engine does **not** auto-recover, because the property that makes auto-retry
  sound elsewhere — durable, fenced inputs — is exactly what a Shared service lacks: a fresh
  (empty) instance would give a green retry over corrupt state. Honest recovery means re-running
  every writer from the first, against a fresh instance — which *is* a new Take, i.e. a **human
  Rerun** (the per-Take model already provisions a fresh instance and re-runs from the chosen
  point). Auto-forking a Take would blur the machine-vs-human Take boundary
  ([0056](0056-run-takes-and-attempt-grain-evidence.md)) and risk the forward-progress invariant
  (a crash-looping service would auto-rerun forever).

### Evidence / ontology

Neither form is a DAG node (amends [0008](0008-step-contract.md)).

- **Sidecar** logs are just another container under the host Step's logs (multi-container fold);
  its lifecycle is the Step's.
- **Shared service** is a **Run-scoped, non-DAG resource** keyed `{run, take, service}` with its own
  lifecycle status (`starting → ready → running → torn-down | failed`) and a best-effort log stream
  ([0013](0013-history-and-observability.md)), surfaced in a **"Services" panel beside the DAG** —
  never as a node inside it. This gives operators the "why won't my DB come up" logs without
  polluting the graph.

## Consequences

- The 95% case (one test Step + a throwaway DB) gets the **light, fenced, zero-networking** Sidecar
  shape. The rare shared-live-DB case gets an honest, explicitly-unfenced tool.
- **New surface:** pipeline-level `services:` and per-Step `services:`/`uses:` IR fields; a
  readiness-probe schema; scheduler readiness-gating on a **non-Step** resource; a Run-scoped
  service resource with status + best-effort logs + a UI Services panel; per-Take service
  instancing and teardown; network-policy scoping for opt-in Pods; governed-grant keying on service
  image digests.
- **Local executor:** `scarab-executor-local` is a **host-process spawner** with no container
  runtime, so it **rejects** container-image services with direction (mirroring how it already
  rejects `clone`/`build`). Both service forms run on the **k8s executor** — which is what a local
  dogfood uses against kind (`just up`) or colima (`just local-helm`). Sidecar = an extra co-located
  Pod container; Shared = a Pod + Service.
- **Matrix:** a matrixed Step's Sidecars multiply per instance (each instance's Pod gets its own);
  Shared services are pipeline-level and are **not** multiplied by a Step's matrix.
- The durable data plane ([0007](0007-data-passing-model.md)) remains the sanctioned way to pass
  state between steps; a Shared DB is explicitly *scaffolding*, not a state channel.

## Alternatives considered

- **Shared-across-steps as the only form** — rejected: even a lone test Step would pay cross-Pod
  DNS, a Run-scoped durable resource, and the loss of fenced isolation for what a co-located sidecar
  does lighter and safer.
- **Co-schedule a subgraph into one Pod (GHA "job = machine")** — rejected: the subgraph becomes one
  unit of re-execution, surrendering per-Step restart — the durability wedge.
- **Ambient run-wide reachability (Woodpecker/GHA)** — rejected: with per-Pod network isolation,
  ambient means a network hole for every Pod; explicit `uses:` scopes it to opt-in Pods.
- **Bounded auto-restart of a dead Shared service** — rejected: on an `emptyDir` the restart is
  silently empty, so a step that seeded the DB reads a blank one — silent corruption, worse than
  failing.
- **Engine auto-rerun (auto-fork a Take) on mid-run death** — rejected: unfenced non-durable state
  can't be soundly re-derived by step-level retry; auto-forking blurs the machine/human Take
  boundary and risks a crash-loop against forward-progress.
- **Keep `service` a formal step kind for uniformity** — rejected: false uniformity. A non-node
  forced into a DAG node has no coherent rerun action or status pill.
- **Readiness left to the step author (`sleep`)** — rejected: with a durable scheduler, "wait until
  healthy" is a native suspend; punting reproduces Woodpecker's `sleep 30s` folklore.
