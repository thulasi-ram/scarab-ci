// Status pill (docs/DESIGN.md §4). succeeded → ok, failed → danger, skipped →
// muted; running/pending → teal-green with a copper hairline (the one place a
// copper edge signals "in motion"). Terminal-state mapping (ADR-0047):
// dead_lettered (the operator signal) and cancelled render with the danger
// style; suspended (waiting on a gate) renders as in-motion. Unknown statuses
// fall back to skipped.
const CLASS_OF: Record<string, string> = {
  succeeded: "succeeded",
  failed: "failed",
  dead_lettered: "failed",
  cancelled: "failed",
  running: "running",
  pending: "pending",
  suspended: "pending",
  skipped: "skipped",
};

export default function StatusBadge(props: { status: string }) {
  const cls = () => CLASS_OF[props.status] ?? "skipped";
  return <span class={`badge badge-${cls()}`}>{props.status}</span>;
}
