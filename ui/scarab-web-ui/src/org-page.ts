// Pure rules behind the org page: its address, the counts in its stat tiles,
// and the paging of its repository list. No imports beyond gates.ts (itself
// import-free), so the mock fixture can use the paging too and the no-DOM
// vitest tier covers all of it.
import { inboxRuns, type InboxRunLike } from "./gates";

/**
 * The org page. Under the static `/orgs/` segment, beside the org's settings
 * (`/orgs/<org>/settings/…`), so it cannot collide with the two-segment
 * `/:org/:repo` tier: `orgs` is a reserved org name on the server.
 */
export const orgPageHref = (org: string) => `/orgs/${encodeURIComponent(org)}`;

/** What the run counts read from a run summary. */
export type OrgRunLike = InboxRunLike & { org?: string | null; created_at: number };

export interface OrgRunStats {
  /** The org's runs created in the last 24 hours. */
  lastDay: number;
  /** The org's runs created in the last 7 days. */
  lastWeek: number;
  /** The org's runs created in the last 24 hours that failed. */
  failedLastDay: number;
  /** The org's runs waiting on a gate, of any age. */
  waiting: number;
  /**
   * The counts may be short: the list was cut at `limit` runs and its oldest
   * run is younger than a week, so older runs of the week were not fetched.
   */
  partial: boolean;
}

const DAY_MS = 24 * 60 * 60 * 1000;
const WEEK_MS = 7 * DAY_MS;

/**
 * Count `org`'s runs among `runs`, the org's most recent runs
 * (`GET /v1/runs?org=&limit=`; runs of other orgs are ignored). `limit` is
 * the size that was asked for, so a full list whose oldest run falls inside
 * the week is reported as partial.
 */
export function orgRunStats<T extends OrgRunLike>(
  runs: readonly T[],
  org: string,
  now: number,
  limit: number,
): OrgRunStats {
  const mine = runs.filter((r) => r.org === org);
  const inDay = mine.filter((r) => now - r.created_at <= DAY_MS);
  const oldest = runs.reduce((min, r) => Math.min(min, r.created_at), Infinity);
  return {
    lastDay: inDay.length,
    lastWeek: mine.filter((r) => now - r.created_at <= WEEK_MS).length,
    failedLastDay: inDay.filter((r) => r.status === "failed" || r.status === "dead_lettered").length,
    waiting: inboxRuns(mine).length,
    partial: runs.length >= limit && now - oldest <= WEEK_MS,
  };
}

/** How many pages `total` items fill; at least one, so an empty list has a page. */
export function pageCount(total: number, perPage: number): number {
  if (perPage <= 0) return 1;
  return Math.max(1, Math.ceil(total / perPage));
}

/** `page` moved into `1..=pageCount`. */
export function clampPage(page: number, total: number, perPage: number): number {
  return Math.min(Math.max(1, Math.floor(page)), pageCount(total, perPage));
}

/**
 * The 1-based positions a page shows, for "26–32 of 32". `{ from: 0, to: 0 }`
 * when there is nothing to show.
 */
export function pageSpan(page: number, perPage: number, total: number): { from: number; to: number } {
  if (total <= 0 || perPage <= 0) return { from: 0, to: 0 };
  const p = clampPage(page, total, perPage);
  const from = (p - 1) * perPage + 1;
  return { from, to: Math.min(total, p * perPage) };
}

/** One page of `items`; past the last page, nothing. */
export function paginate<T>(items: readonly T[], page: number, perPage: number): T[] {
  if (perPage <= 0) return [];
  const start = (Math.max(1, Math.floor(page)) - 1) * perPage;
  return items.slice(start, start + perPage);
}
