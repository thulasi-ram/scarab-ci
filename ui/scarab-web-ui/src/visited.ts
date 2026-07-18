// Recently-visited repos — a browser-local signal (there is no server-side visit
// concept). Written on every RepoView mount, read by the dashboard's "recently
// visited" row. Most-recent first, deduped, capped.
const KEY = "scarab-visited-repos";
const MAX = 8;

export type VisitedRepo = { org: string; project: string };

export function getVisited(): VisitedRepo[] {
  try {
    const raw = localStorage.getItem(KEY);
    const list = raw ? (JSON.parse(raw) as VisitedRepo[]) : [];
    return Array.isArray(list) ? list.filter((v) => v && v.org && v.project) : [];
  } catch {
    return [];
  }
}

export function recordVisit(org: string, project: string): void {
  try {
    const next = [{ org, project }, ...getVisited().filter((v) => !(v.org === org && v.project === project))];
    localStorage.setItem(KEY, JSON.stringify(next.slice(0, MAX)));
  } catch {
    /* storage disabled — no history, degrades to the active-repos fallback */
  }
}
