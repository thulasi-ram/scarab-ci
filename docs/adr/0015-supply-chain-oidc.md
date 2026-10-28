# 0015. Supply-chain: Scarab as OIDC issuer + provenance-by-construction

- **Status:** Accepted
- **Date:** 2026-07-12
- **Deciders:** thulasi.ram (architect)

> **Status note (2026-10-28):** the OIDC issuer half is built (`crates/scarab-server/src/oidc.rs`).
> The provenance half (attestation capture, SLSA, cosign, SBOM export) is not built yet.

## Context

Because the DAG is durable, content-addressed, and records what produced what, two
security features take *little extra work*, where most CIs bolt them on
painfully.

## Decision

**v1:**

- **Scarab is an OIDC issuer** for **keyless cloud federation.** Mint short-lived per-run JWTs
  with claims (`repo`, `ref`, `environment`, `pipeline`) that cloud IAM trust policies key on,
  so pipelines authenticate to AWS/GCP/Azure/registries with **zero stored long-lived
  secrets** (à la GHA OIDC). Works with keyless signing.
- **Provenance-by-construction hooks:** we already record the full "source + steps + inputs →
  artifact" graph; capture it so SLSA attestation is mostly serialization of existing state.

**Fast-follow:** SLSA attestation export, keyless **cosign** signing (via our own OIDC
issuer), SBOM attach.

## Consequences

- Env-scoped OIDC subjects tie into Environments ([0024](0024-environments.md)).
- Fewer stored secrets ([0014](0014-secrets.md)) → smaller blast radius.
- High value for the effort; gets a differentiator in early without over-scoping v1.

## Alternatives considered

- **All-in v1 (OIDC + SLSA + cosign + SBOM)** — strongest pitch, notably more v1 scope.
- **Defer all** — gives up a differentiator that costs almost nothing; retrofitting OIDC trust is friction.
