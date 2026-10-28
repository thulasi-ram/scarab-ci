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
