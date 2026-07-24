# Unimplemented / stub sweep — 2026-07-24

Three-way sweep (ADR-vs-code audit, backend prod-path stub hunt, UI/deploy/docs
gap hunt) prompted by the matrix-interpolation escape (`343555f`: a canned
implementation shipped and the feature never ran). Question: what else is
mock, stubbed, or "implement later"?

## Headline

**The code is in better shape than the docs claim.** No surviving silent
production fake of the matrix kind was found. Secrets encryption (real
AES-256-GCM envelope), matrix expansion, BuildKit + registry auth, sidecar
SIGTERM drain, orphan-Pod teardown, log-tail backoff, ADR-0055/0056/0057/0058
(both service halves) — all verified genuinely implemented. The dominant
finding is the inverse problem: **stale documentation under-reporting shipped
features.**

## Real gaps (ranked)

1. **ADR-0015 attestation half — the only open code gap.** OIDC issuer is
   real (`scarab-server/src/oidc.rs` — per-attempt RS256 JWTs, JWKS,
   discovery). SLSA/SBOM/cosign/provenance-export: zero code hits. Unchanged
   since the 2026-07-19 audit; tracked in `docs/followups.md` (demand-gated).
2. **ADR-0059 tick fault isolation — Proposed, correctly unbuilt, but the
   one open engine-correctness item.** Only Fix B (collect-and-continue in
   `reconcile_services`, `scheduler.rs:914-917`) landed; `admit`/`advance`/
   other `reconcile*` still abort the whole tick on one poison run, and
   swallowed service-reconcile errors can hot-loop unbounded. Was missing
   from followups.md (now added).
3. **Auth bypass path (marked, opt-in):** with no session store,
   `authenticate` returns a synthetic Owner for every caller
   (`scarab-server/src/lib.rs:4568`). Gated behind `SCARAB_DEV_INSECURE`,
   default-deny otherwise, loud boot warning — acceptable, but it is the
   most permissive path in prod code and worth a periodic re-check.
4. **`TODO(slice-2)` engine features (marked, safe):**
   - explicit workspace `inputs:`/`outputs:` selection unimplemented — a
     step always inherits ALL of its `needs`' snapshots
     (`scarab-engine/src/lib.rs:886`). Pipeline-level tests exercise the
     parse; verify the fields aren't silently dropped IR→engine.
   - content-addressed rerun skip (ADR-0027 optimisation) falls back to the
     safe full cascade (`scheduler.rs:160`). Never wrong, just more work.
5. **Helm: no first-class Forgejo webhook secret** — chart renders
   `SCARAB_GITHUB_WEBHOOK_SECRET` but Forgejo's must be smuggled via
   `existingSecret` (`deploy/helm/scarab/templates/secret.yaml:24-26`);
   tracked as git-bug `f3da2aa`.
6. **Debug-shell button is a conditional dead affordance:** enabled whenever
   a step is selected, but `/attach`//`debug-pod` 404 on non-k8s executor
   deploys — user discovers only via a socket error
   (`RunDetail.tsx:678-693`, `DebugShell.tsx:37-45`). Cheap fix: hide/tooltip
   when the deploy can't serve it.
7. **Dogfood ci.yaml is deliberately minimal** (checks one crate, no
   caching) and the deferral is genuine: no build/layer cache exists in the
   product to adopt — workspace CAS restores *source*, not compile
   artifacts; `CARGO_TARGET_DIR` is ephemeral. Real future work under
   ADR-0018's cache story.
8. Cosmetic, marked: command-palette repo search is client-side only
   (`CommandPalette.tsx:2-3`); two client calls bypass the generated OpenAPI
   client (`client.ts:658-674`).

## Stale docs corrected / to correct (code is ahead of them)

- `docs/followups.md` claimed **invoke (ADR-0038) "does not exist in code"**
  — it is fully built (`inline_invokes`, `scarab-pipeline/src/lib.rs:838-842`).
  Corrected in this branch; only remote-invoke, submodule vendoring, and the
  data→IR frontend remain deferred.
- **ADR-0018 amendment (2026-07-17)** said `build_pod_for_build` is unwired
  with no registry auth — both are wired (`build_pod` +
  `ensure_registry_secret`, explicit + forge-derived). Corrected in this
  branch.
- **`scarab-cli/src/main.rs:7`** header claims subcommands other than `run`
  are "compiling stubs" — `lint`/`validate`/`logs`/`rerun` are all real.
  Code-comment fix; belongs in a code PR, not this docs branch.
- ADR pages rendered in the docs site quote proposal-time text ("retry
  classification entirely unimplemented", "8× unimplemented!() in clone",
  "runs_on stubbed") that no longer matches code — `unimplemented!()` has
  zero prod hits and `runs_on` no longer exists (replaced by ADR-0055
  placement). Historical ADR prose, not live gaps.

## Explicitly cleared (checked, no gap)

Sidecar drain-on-SIGTERM (`docker/sidecar/scarab-results-egress.sh:49-59`);
orphan-Pod TOCTOU teardown (`scheduler.rs:369-392`); log-tail hot-loop
backoff (`log_tail.rs:287-294`); run budget enforcement; mock-mode leakage
into the real UI path (none — dynamic import, tree-shaken); UI dead handlers
(none); helm RBAC/ingress/servicemonitor completeness; forge adapter no-ops
that are by-design (GitHub `register_webhook`, Forgejo `create_deployment`).
