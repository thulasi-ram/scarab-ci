// Trigger-kind presentation, shared by the run detail bar and the runs list so
// "trigger" reads the same everywhere. The kind (push / pull_request / manual /
// …) is distinct from the Actor who caused it ("triggered by").

/** Display token for a trigger kind (`pull_request` → `PR`). `run` when absent. */
export function triggerText(kind?: string | null): string {
  if (!kind) return "run";
  return kind === "pull_request" ? "PR" : kind;
}

/** An icon that reads for the trigger kind (from the small brand icon set). */
export function triggerIcon(kind?: string | null): string {
  switch (kind) {
    case "pull_request":
      return "git-pull-request";
    case "push":
      return "git-commit-horizontal";
    case "tag":
    case "release":
      return "tag";
    case "manual":
    case "api":
      return "play";
    default:
      return "circle-dot";
  }
}
