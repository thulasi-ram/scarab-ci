# 0015. Supply-chain: Scarab as OIDC issuer + provenance-by-construction

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

## Context

Because the DAG is durable, content-addressed, and records exactly what produced what, two
modern security features are *nearly free* — a differentiator most CIs bolt on painfully.

## Decision

**v1:**

- **Scarab is an OIDC issuer** for **keyless cloud federation.** Mint short-lived per-run JWTs
  with claims (`repo`, `ref`, `environment`, `pipeline`) that cloud IAM trust policies key on,
  so pipelines authenticate to AWS/GCP/Azure/registries with **zero stored long-lived
  secrets** (à la GHA OIDC). Composes with keyless signing.
- **Provenance-by-construction hooks:** we already record the full "source + steps + inputs →
  artifact" graph; capture it so SLSA attestation is mostly serialization of existing state.

**Fast-follow:** SLSA attestation export, keyless **cosign** signing (via our own OIDC
issuer), SBOM attach.

## Consequences

- Env-scoped OIDC subjects tie into Environments ([0024](0024-environments.md)).
- Fewer stored secrets ([0014](0014-secrets.md)) → smaller blast radius.
- High value/effort ratio; banks a differentiator early without over-scoping v1.

## Alternatives considered

- **All-in v1 (OIDC + SLSA + cosign + SBOM)** — strongest story, notably more v1 surface.
- **Defer all** — forfeits an almost-free differentiator; retrofitting OIDC trust is friction.
