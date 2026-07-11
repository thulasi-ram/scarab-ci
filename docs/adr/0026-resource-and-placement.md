# 0026. Resource & placement model

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

Tension: the IR should stay **portable/agnostic** ([0009](0009-dsl-ir-yaml-cel.md)), but
placement is inherently k8s-ish (nodeSelector, tolerations, runtimeClass, resource requests,
GPU). Letting pipelines write raw k8s couples the IR and leaks infra into pipeline files;
over-abstracting blocks power users (a specific taint, a GPU class).

## Decision

**Abstracted labels in the IR + admin-configured mapping + a raw escape hatch:**

- Pipelines say `runs-on: linux/arm64`, `resources: { cpu, mem }`, optional `size: large`.
- A **cluster-admin config** maps those labels to k8s specifics (nodeSelector / tolerations /
  runtimeClass / requests).
- A **raw pod-spec overlay** is available for power users who truly need it.

Portable by default; no ceiling. Infra lives in admin config, not pipeline files.

## Consequences

- IR stays agnostic and multi-frontend-friendly; pipelines are clean.
- Admins own the label→k8s mapping (one place).
- The raw overlay is an explicit, discouraged-by-default escape hatch.

## Alternatives considered

- **Raw k8s in the IR** — max power, but IR k8s-coupled; infra leaks into every pipeline.
- **Fixed sizes only** — cleanest, but no escape hatch for niche needs.
