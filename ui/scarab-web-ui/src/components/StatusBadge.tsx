// Status pill (docs/DESIGN.md §4). succeeded → ok, failed → danger, skipped →
// muted; running/pending → teal-green with a copper hairline (the one place a
// copper edge signals "in motion"). Terminal-state mapping (ADR-0047):
// dead_lettered (the operator signal) and cancelled render with the danger
// style; suspended (waiting on a gate) renders as in-motion. Unknown statuses
// fall back to skipped.
import Pill, { type PillTone } from "./Pill";

const TONE_OF: Record<string, PillTone> = {
  succeeded: "ok",
  failed: "danger",
  dead_lettered: "danger",
  cancelled: "danger",
  running: "motion",
  pending: "motion",
  suspended: "motion",
  skipped: "muted",
};

export default function StatusBadge(props: { status: string }) {
  return (
    <Pill tone={TONE_OF[props.status] ?? "muted"} look="solid">
      {props.status}
    </Pill>
  );
}
