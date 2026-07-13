// Status pill (docs/DESIGN.md §4). succeeded → ok, failed → danger, skipped →
// muted; running/pending → teal-green with a copper hairline (the one place a
// copper edge signals "in motion"). Unknown statuses fall back to skipped.
const KNOWN = new Set(["succeeded", "failed", "running", "pending", "skipped"]);

export default function StatusBadge(props: { status: string }) {
  const cls = () => (KNOWN.has(props.status) ? props.status : "skipped");
  return <span class={`badge badge-${cls()}`}>{props.status}</span>;
}
