// The Runs tab's server-side filters (ADR-0074 §2): Environment, and the
// deploy marker (a pipeline or Step marked `deploy: true`). Both run on the
// server, so the rows on screen are already filtered — the Environment
// picker's options can't come from them alone, or picking one Environment
// would hide every other.

/** The Environment picker's options: the Project's Environments, every
 *  Environment a loaded run is governed by (a run can outlive the Project's
 *  eligibility for its Environment), and the selected one (a shared link can
 *  name an Environment neither list has). Distinct, non-empty, sorted. */
export function runEnvironmentOptions(
  projectEnvironments: readonly string[],
  runs: readonly { environment?: string | null }[],
  selected: string,
): string[] {
  const seen = new Set<string>();
  for (const e of projectEnvironments) if (e) seen.add(e);
  for (const r of runs) if (r.environment) seen.add(r.environment);
  if (selected) seen.add(selected);
  return [...seen].sort();
}

/** The runs request's server-side filter: the selected Environment (omitted
 *  when none — never `""`, which the server rejects with a 400), and
 *  `deploy: true` when the Deploys toggle is on (omitted otherwise, so the
 *  list is every run rather than only the non-deploys). */
export function runListFilter(
  environment: string,
  deploysOnly: boolean,
): { environment?: string; deploy?: true } {
  return {
    ...(environment ? { environment } : {}),
    ...(deploysOnly ? { deploy: true as const } : {}),
  };
}

/** A row of the Runs list: a Run, or a trigger that was refused before it
 *  became one (ADR-0076). */
export type RunListRow<R, F> = { kind: "run"; run: R; at: number } | { kind: "refused"; refused: F; at: number };

/** The bare pipeline name of a refused Run's file (`.scarab/ci.yaml` → `ci`),
 *  the name a Run's `pipeline` carries. */
export function barePipeline(path: string): string {
  return path.replace(/^\.scarab\//, "").replace(/\.ya?ml$/, "");
}

/** The Runs list with refused triggers between the Runs, newest first.
 *
 *  - `refused` status shows only refusals; `running` and `failed` only Runs.
 *  - A refusal older than the oldest loaded Run is left out: the page shows
 *    a window of time, and an old refusal at its foot would read as recent.
 *  - The author and pipeline filters apply to refusals too; a filter a
 *    refusal cannot match (an Environment, deploys, a PR) leaves them out. */
export function mergeRefused<
  R extends { created_at: number },
  F extends { created_at: number; actor?: string | null; pipeline: string },
>(
  runs: readonly R[],
  refused: readonly F[],
  f: { status: string; author: string; pipeline: string; runOnly: boolean },
): RunListRow<R, F>[] {
  const runRows: RunListRow<R, F>[] =
    f.status === "refused" ? [] : runs.map((run) => ({ kind: "run", run, at: run.created_at }));
  const oldest = runs.length ? Math.min(...runs.map((r) => r.created_at)) : -Infinity;
  const keepRefused = (f.status === "all" || f.status === "refused") && !f.runOnly;
  const refusedRows: RunListRow<R, F>[] = keepRefused
    ? refused
        .filter((r) => f.status === "refused" || r.created_at >= oldest)
        .filter((r) => !f.author || r.actor === f.author)
        .filter((r) => !f.pipeline || barePipeline(r.pipeline) === f.pipeline)
        .map((refused) => ({ kind: "refused", refused, at: refused.created_at }))
    : [];
  return [...runRows, ...refusedRows].sort((a, b) => b.at - a.at);
}
