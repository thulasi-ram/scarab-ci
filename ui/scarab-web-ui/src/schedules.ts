// The repo page's Schedules tab: how a schedule's last fire reads, and the
// Runs tab's scheduled filter that a schedule links to.

/** The trigger kind the server stamps on a Run a schedule started. */
export const SCHEDULED_TRIGGER = "cron";

/** Whether a Run, or a refused trigger, came from an `on: cron:` schedule. */
export function isScheduled(triggerKind?: string | null): boolean {
  return triggerKind === SCHEDULED_TRIGGER;
}

/** The Runs tab showing only the scheduled Runs of one Pipeline: the
 *  `?scheduled=1` filter and the Pipeline's short name as `?pipeline=`. */
export function scheduledRunsHref(org: string, repo: string, pipelinePath: string): string {
  const q = new URLSearchParams({ scheduled: "1", pipeline: pipelineShortName(pipelinePath) });
  return `/${org}/${repo}?${q}`;
}

/** A Pipeline path as the runs list names it: `.scarab/nightly.yaml` → `nightly`. */
export function pipelineShortName(path: string): string {
  return path.replace(/^\.scarab\//, "").replace(/\.ya?ml$/, "");
}

/** A last fire's outcome as shown: its label, and the `rr-status` class that
 *  colours it. A slot that was refused or missed is in the refused list too,
 *  so it reads as a failure; an outcome this UI does not know shows as sent. */
export function fireOutcome(outcome: string): { label: string; tone: string } {
  switch (outcome) {
    case "pending":
      return { label: "pending", tone: "pending" };
    case "run":
      return { label: "ran", tone: "" };
    case "skipped":
      return { label: "skipped", tone: "" };
    case "refused":
      return { label: "refused", tone: "failed" };
    case "missed":
      return { label: "missed", tone: "failed" };
    default:
      return { label: outcome, tone: "" };
  }
}
