// Settings → Event log (git-bug fa23028): the org's security event log on a
// page of its own, one server page at a time, with its filters in the URL so
// a filtered view can be shared.
//
// Pure — no DOM, no fetch — so the no-DOM vitest tier covers how the URL
// becomes a request and how the form becomes a URL.
//
// The URL holds `kind`, `actor`, `target`, `from`, `to` and `cursor`. Times
// are UTC instants in ISO form (`2026-11-01T09:30Z`), so a shared link means
// the same moment wherever it is opened; the form shows them in local time.

/** The filters, as the URL holds them (each `""` when unset). */
export type EventLogFilter = {
  kind: string;
  actor: string;
  target: string;
  from: string;
  to: string;
};

/** The URL's view of the log: its filters and the page it is on. */
export type EventLogView = EventLogFilter & { cursor: string };

/** What `useSearchParams` hands back. */
export type SearchLike = Record<string, string | string[] | undefined>;

const one = (v: string | string[] | undefined): string =>
  (Array.isArray(v) ? v[0] : v)?.trim() ?? "";

/** The view a URL asks for. */
export function readView(search: SearchLike): EventLogView {
  return {
    kind: one(search.kind),
    actor: one(search.actor),
    target: one(search.target),
    from: one(search.from),
    to: one(search.to),
    cursor: one(search.cursor),
  };
}

/** An instant from the URL as unix ms; `undefined` when unset or unreadable. */
export function parseInstant(s: string): number | undefined {
  if (!s) return undefined;
  const ms = Date.parse(s);
  return Number.isFinite(ms) ? ms : undefined;
}

/** The request for a view: only what is set, times as unix ms. An
 *  unreadable time is left out rather than sent. */
export function eventsQuery(v: EventLogView): {
  kind?: string;
  actor?: string;
  target?: string;
  from?: number;
  to?: number;
  cursor?: string;
} {
  const from = parseInstant(v.from);
  const to = parseInstant(v.to);
  return {
    ...(v.kind ? { kind: v.kind } : {}),
    ...(v.actor ? { actor: v.actor } : {}),
    ...(v.target ? { target: v.target } : {}),
    ...(from !== undefined ? { from } : {}),
    ...(to !== undefined ? { to } : {}),
    ...(v.cursor ? { cursor: v.cursor } : {}),
  };
}

/** The search params for a changed filter. Any filter change goes back to
 *  the newest page: a cursor belongs to the filter it was issued under. An
 *  empty value clears its param. */
export function filterSearch(f: EventLogFilter): Record<string, string> {
  return { kind: f.kind, actor: f.actor.trim(), target: f.target.trim(), from: f.from, to: f.to, cursor: "" };
}

/** Is any filter set? */
export function isFiltered(f: EventLogFilter): boolean {
  return !!(f.kind || f.actor || f.target || f.from || f.to);
}

/** What the time range is wrong about, or `null`. */
export function rangeProblem(f: EventLogFilter): string | null {
  if (f.from && parseInstant(f.from) === undefined) return "The From time is not a date.";
  if (f.to && parseInstant(f.to) === undefined) return "The To time is not a date.";
  const from = parseInstant(f.from);
  const to = parseInstant(f.to);
  if (from !== undefined && to !== undefined && from >= to) return "From must be before To.";
  return null;
}

const pad = (n: number) => String(n).padStart(2, "0");

/** A URL instant as a `datetime-local` value, in local time; `""` when unset
 *  or unreadable. */
export function toLocalInput(iso: string): string {
  const ms = parseInstant(iso);
  if (ms === undefined) return "";
  const d = new Date(ms);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** A `datetime-local` value (local time) as the URL's UTC instant, to the
 *  minute; `""` when empty or unreadable. */
export function fromLocalInput(local: string): string {
  if (!local) return "";
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})/.exec(local);
  if (!m) return "";
  const d = new Date(+m[1]!, +m[2]! - 1, +m[3]!, +m[4]!, +m[5]!);
  if (!Number.isFinite(d.getTime())) return "";
  return `${d.toISOString().slice(0, 16)}Z`;
}

/** The empty table's words. */
export function emptyText(f: EventLogFilter, paged: boolean): string {
  if (paged) return "No older events.";
  return isFiltered(f) ? "No events match these filters." : "No security events yet.";
}
