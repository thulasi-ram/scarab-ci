// The repo page's Schedules panel: how a schedule's last fire reads.

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
