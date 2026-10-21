// Status pill (docs/DESIGN.md §4). succeeded → ok, failed → danger, skipped →
// muted; running/pending → teal-green with a copper hairline (the one place a
// copper edge signals "in motion"). Terminal-state mapping (ADR-0047):
// dead_lettered (the operator signal) and cancelled render with the danger
// style; suspended (waiting on a gate) renders as in-motion. Unknown statuses
// fall back to skipped.
//
// Deployment statuses (ADR-0070) share the palette rather than adding one:
// live is the good terminal, in_progress/requested are in motion, stalled is
// red (it missed its deadline, though it can still recover), and superseded is
// muted — a newer Deployment took its place, which is history, not a fault.
const CLASS_OF: Record<string, string> = {
  succeeded: "succeeded",
  failed: "failed",
  dead_lettered: "failed",
  cancelled: "failed",
  running: "running",
  pending: "pending",
  suspended: "pending",
  skipped: "skipped",
  live: "succeeded",
  in_progress: "running",
  requested: "pending",
  stalled: "failed",
  superseded: "skipped",
};

export default function StatusBadge(props: { status: string }) {
  const cls = () => CLASS_OF[props.status] ?? "skipped";
  return <span class={`badge badge-${cls()}`}>{props.status}</span>;
}
