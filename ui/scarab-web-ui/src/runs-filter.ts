// The Runs tab's Environment filter (ADR-0074 §2): a Project's deploys are its
// Runs filtered by Environment. The filter runs on the server, so the rows on
// screen are already one Environment's — the picker's options can't come from
// them alone, or picking one Environment would hide every other.

/** The Environment picker's options: the Project's Environments, every
 *  Environment a loaded run deployed into (a run can outlive the Project's
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

/** The `environment` query value for the runs request: the selected
 *  Environment, or `undefined` for none — never `""`, which the server
 *  rejects with a 400. */
export function environmentQuery(selected: string): string | undefined {
  return selected === "" ? undefined : selected;
}
