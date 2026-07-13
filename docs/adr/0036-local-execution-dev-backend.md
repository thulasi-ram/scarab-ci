# 0036. Local execution: a non-production dev/CLI backend (amends 0005)

- **Status:** Accepted
- **Date:** 2026-07-13
- **Deciders:** thulasi.ram (architect)

## Context

[0005](0005-tenancy-and-k8s-only.md) states **Kubernetes is the ONLY execution
substrate** (explicit non-goal: Docker-socket, local, SSH). Yet a
`scarab-executor-local` crate exists and the roadmap (CONTEXT §9.7, slice 7) lists
"local exec." Read literally these conflict. The `Executor` **port**
([0004](0004-execution-topology.md), [0016](0016-code-architecture.md)) makes
alternative backends *possible* — the durable brain (`scarab-engine` + the
Postgres outbox + the API) never touches Kubernetes — so the question is one of
product stance, not feasibility.

## Decision

**`scarab-executor-local` is a non-production developer / CLI / test backend, not
a supported deployment mode.** [0005]'s "Kubernetes-only" governs how Scarab is
*deployed and run as a service*; it is unchanged. Local execution exists only to:

- **`scarab run` on a laptop** — execute a pipeline as host child processes
  (`tokio::process`, no Docker, no cluster) for a fast inner loop before pushing.
- **Test the real engine end-to-end without a cluster** — drive the actual
  scheduler/state-machine against a process-backed executor in CI of Scarab
  itself. (This is distinct from the *fake* executor, which scripts outcomes.)

It is **never** offered as a hosted/self-hosted execution backend, and carries no
multi-backend abstraction beyond the single `Executor` port that already exists.
"Local dev against a cluster" remains **kind** (Kubernetes-in-Docker) on the k8s
adapter — the real substrate.

Consequences for its semantics (a dev tool, so deliberately narrower than k8s):

- **Idempotent-on-fence within a process** (re-launching the same `{run, step,
  attempt}` re-attaches rather than double-spawning), matching the k8s adapter's
  contract *in-process*. Surviving a control-plane **restart** mid-run is the k8s
  adapter's job (durable Pods); after a local restart an in-flight step reports
  `Lost` and the orchestrator relaunches ([0020](0020-retry-and-failure.md)).
- **No content-addressed workspace / `output`** in v1 — returns `None`; workspace
  CAS is the k8s post-step path. So restart skip-if-unchanged ([0027]) and
  `outputs:` ([0035]) don't engage under local exec (safe cascade), same as the
  k8s adapter until its post-step snapshot lands.
- Steps must carry a `command` (there is no image to default to).

## Consequences

- Fast local iteration and cluster-free engine tests, without weakening the
  k8s-only *product* stance.
- The non-goal in [0005] is clarified, not reversed: "no local backend" means "no
  local *production* backend."

## Alternatives considered

- **Drop host-process exec entirely; `local` == kind** — strictly honors [0005]'s
  letter, but forfeits a cluster-free inner loop and Docker-free engine tests.
- **Promote local to a real backend** — reintroduces the multi-backend baggage
  [0005] exists to shed. Rejected.
