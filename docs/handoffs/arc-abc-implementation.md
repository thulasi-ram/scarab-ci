# Handoff — implement the Arc A/B/C backlog (AFK loop)

The 2026-07-16 production-readiness audit → four+ grilled ADRs (0045–0054, plus
amendments to 0010/0018/0020) → **~38 `ready-for-agent` git-bug tickets**. This
is the ritual for implementing them one-by-one, unattended.

**Read first:** `CONTEXT.md` (ubiquitous language, invariants), the ADR each
ticket cites, and `docs/audits/2026-07-16-production-readiness.md` (why each gap
matters, with file:line anchors).

## ⚠️ Current state — start here (design session left this uncommitted)

- The design output is **uncommitted on `main`**: new ADRs `0045`–`0054`,
  modified `CONTEXT.md` / `docs/adr/0018-image-building.md` / `docs/adr/README.md`,
  `docs/audits/2026-07-16-production-readiness.md`, and this handoff. `git status`
  shows them all.
- The **40 git-bug tickets are local** (not pushed). So **this loop must run on
  THIS checkout** (same machine) — a fresh clone won't have the docs or tickets.
- **First loop action (once):** create `feat/arc-abc-impl` off `origin/main`,
  then **commit the design docs onto it** (`docs(adr): source-provisioning →
  product-surface ADRs + audit + handoff (0045–0054)`) so the base is clean and
  every ticket's cited ADR is present. Only then start implementing tickets.
- Nothing here is pushed and no ADR is `Accepted` yet — implement them as
  written (they're `Proposed`); flag contradictions via `git bug bug comment`.

## The queue

`git bug bug` → tickets titled `[ADR-00xx] …`, labeled `ready-for-agent`. Pick the
highest-value **unblocked** ticket — one whose every "Blocked by" id is
**closed** — in dependency order. Suggested spine (foundational first):

1. **`[ADR-0048]` config + fail-closed** (`39e40c3` → `2c53d20`) — the config
   module everything else reads.
2. **`[ADR-0047]` A1 classification** (`e0eed95`) → A2 (`2ea455a`) → A3
   (`a7b2d75`) → A4/A5/A6 → A7 — the durable core.
3. **`[ADR-0046]` rename** (`fd4aeb2`) → model refactor (`303ea80`) → ForgePort
   (`2ee8613`) → registry (`35cadd8`) → GitHub (`d54e9be`)/Forgejo (`1399b78`) →
   routing (`ec0b2eb`) → wire-to-prod (`6c455e8`).
4. **`[ADR-0045]` clone** (`c4856cd`, `c353bcd`, `ddead23` → …) — needs the
   forge token path (0046) for private repos; the public-repo acceptance
   (`5c78a43`) does not.
5. **`[ADR-0049]` C1 authn** (`aba95c5`) → C2 RBAC (`809df57`); then the rest of
   Arc C (0050–0054) — mostly independent.

Blocked/other-arc tickets: `git bug bug show <id>` to read the body + acceptance
criteria + blockers.

## Per-ticket ritual

1. **Branch once:** work on `feat/arc-abc-impl`, created off **`origin/main`**
   (local `main` may be stale). Commit each ticket sequentially onto it so
   dependents build on prior work. **Do not push, do not open PRs** — the human
   reviews the branch on return.
2. **Implement** to the ticket's acceptance criteria. Honor hexagonal purity
   (ADR-0016/0031 — pure crates import no infra) and the ubiquitous language.
3. **Keep green:** `cargo check --workspace` + `cargo clippy` clean after every
   ticket.
4. **Test** per ADR-0017 (classical, minimal, grow from real behavior): real
   Postgres via `SCARAB_TEST_DATABASE_URL`; mock only true externals;
   cluster/BuildKit/GitHub/Forgejo/UI live-runs are `#[ignore]` +
   `SCARAB_TEST_KUBE=1`-gated.
5. **Verify it actually works** — drive the change, don't just typecheck. For
   live k8s use **Colima only**: `colima start --kubernetes` if needed, and
   **assert `kubectl config current-context` == `colima`** before *any* cluster
   op. ⚠️ The kubeconfig also holds real **ACME prod/staging EKS** contexts —
   never target them. Tear down what you start.
6. **Commit:** `<type>(<area>): <subject>` + a body explaining the decision, with
   trailer `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.
7. **Close:** `git bug bug status close <id>`.

## When a ticket needs something you can't provide

Some tickets need a live external system (real **GitHub App** credentials, a
**Forgejo** instance, a registry). For those: implement the code + adapter,
write the live test `#[ignore]`+gated, add a `git bug bug comment` noting exactly
what a human must verify, commit, and **move on** — do not block the loop.

## Stop conditions

Stop the loop when: the queue has no unblocked ticket left, or everything
remaining is blocked on human input (live creds/instances), or `cargo check`
can't be made green without a design decision. Leave a summary commit/comment.

## Starting the AFK loop (from a fresh session)

Paste this into a new session to run the backlog self-paced, one ticket per
iteration:

```
/loop Implement the Scarab Arc A/B/C backlog one git-bug ticket per iteration, following docs/handoffs/arc-abc-implementation.md exactly. Each iteration: (1) ensure you are on branch feat/arc-abc-impl, created off origin/main if missing (never commit to main, never push, never open PRs); (2) pick the highest-value UNBLOCKED ready-for-agent ticket (all its "Blocked by" ids closed) following the dependency spine in the handoff (0048 config → 0047 durability → 0046 forge → 0045 clone → 0049 identity → rest of Arc C); (3) read the ticket body + the ADR it cites + relevant CONTEXT.md; (4) implement to the acceptance criteria, honoring hexagonal purity (ADR-0016/0031); (5) keep cargo check --workspace + clippy clean; (6) test per ADR-0017 (real Postgres via SCARAB_TEST_DATABASE_URL, mock only true externals, cluster/GitHub/Forgejo/UI live-runs #[ignore]+SCARAB_TEST_KUBE gated); (7) verify it actually works — for live k8s use Colima ONLY and assert kubectl current-context is colima first, NEVER the ACME EKS contexts, tear down what you start; (8) commit onto feat/arc-abc-impl as <type>(<area>): <subject> with a body + trailer "Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"; (9) git bug bug status close <id>. If a ticket needs a live external you cannot provide (real GitHub App creds, a Forgejo instance), implement the code + #[ignore] live test + a git bug comment on what a human must verify, then move on. Stop the loop when no unblocked ticket remains or something needs a human design decision.
```

(Omitting an interval makes it self-pace. For a durable cloud run instead, use
`/schedule` with the same prompt.)

## Safety rails (non-negotiable)

- Never target the ACME EKS contexts; Colima only, context-checked.
- Never push or open PRs; never touch `main` directly.
- ADRs are **Proposed** — implement them as written; if a ticket contradicts its
  ADR or reality, leave a `git bug bug comment` and skip rather than guess.
