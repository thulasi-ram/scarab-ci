// Display derivations for the Deployment record (ADR-0070), kept pure so the
// Deployments page and its tests agree on what each row says.
//
// The record's vocabulary (CONTEXT.md §4.6): a Deployment is a version made, or
// asked to be made, live on a DeploymentTarget. Two display terms are computed
// by the server and only *shown* here — never re-derived:
//   - Unexplained — no Deploy Run requested it; something outside Scarab
//                   changed what is live.
//   - Unconfirmed — requested on a target its source has never reported on, so
//                   nothing will ever move it past `requested` by itself.
// A superseded Deployment additionally says whether it was ever live: "replaced
// after running" and "replaced before it ever took" are different stories, and
// the status alone (`superseded`) cannot tell them apart.
import type { BoardRow, Deployment, DeploymentRequester, DeploymentTarget } from "./api/client";

/** A small label beside a Deployment's status badge. `tone` picks the colour:
 *  `warn` for something an operator should look at, `muted` for context. */
export type DeploymentTag = { key: string; label: string; title: string; tone: "warn" | "muted" };

/** The tags a Deployment row carries, in display order. */
export function deploymentTags(
  d: Pick<Deployment, "status" | "unexplained" | "unconfirmed" | "was_ever_live">,
): DeploymentTag[] {
  const tags: DeploymentTag[] = [];
  if (d.unexplained) {
    tags.push({
      key: "unexplained",
      label: "unexplained",
      title: "No Deploy Run requested this — it was changed outside Scarab.",
      tone: "warn",
    });
  }
  if (d.unconfirmed) {
    tags.push({
      key: "unconfirmed",
      label: "unconfirmed",
      title: "Requested on a target its source has never reported on.",
      tone: "warn",
    });
  }
  if (d.status === "superseded") {
    tags.push(
      d.was_ever_live
        ? { key: "was-live", label: "was live", title: "Live until a newer Deployment replaced it.", tone: "muted" }
        : {
            key: "never-live",
            label: "never went live",
            title: "Replaced before it was ever reported live.",
            tone: "muted",
          },
    );
  }
  return tags;
}

/** A target as one short string: `source/id` (both opaque, source-supplied). */
export function targetLabel(t: Pick<DeploymentTarget, "source" | "id">): string {
  return `${t.source}/${t.id}`;
}

/** The run page a requester points at, or `null` when its Project is unknown —
 *  a run URL needs `/{org}/{project}`, and guessing the project would link to
 *  the wrong repo's run page. */
export function requesterHref(org: string, r: Pick<DeploymentRequester, "run" | "project">): string | null {
  if (!r.project) return null;
  return `/${encodeURIComponent(org)}/${encodeURIComponent(r.project)}/runs/${encodeURIComponent(r.run)}`;
}

/** The board's "in flight" cell: the newest Deployment when it is not the one
 *  that is live (a rollout under way, or one that failed or stalled). */
export function inFlight(row: Pick<BoardRow, "live" | "newest">): Deployment | null {
  const newest = row.newest ?? null;
  if (!newest) return null;
  return row.live && row.live.id === newest.id ? null : newest;
}

/** Board rows grouped by Environment: environments by name, targets by label
 *  within each — so the board reads the same on every load. */
export function groupBoard(rows: readonly BoardRow[]): { environment: string; rows: BoardRow[] }[] {
  const by = new Map<string, BoardRow[]>();
  for (const r of rows) {
    const env = r.target.environment;
    const list = by.get(env);
    if (list) list.push(r);
    else by.set(env, [r]);
  }
  return [...by.keys()]
    .sort((a, b) => a.localeCompare(b))
    .map((environment) => ({
      environment,
      rows: by
        .get(environment)!
        .slice()
        .sort((a, b) => targetLabel(a.target).localeCompare(targetLabel(b.target))),
    }));
}

/** The Org the page reads: the one the caller administers, else the first org
 *  any visible Project belongs to (a member who administers nothing still
 *  reads their org's Deployments). `undefined` = no org yet. */
export function pickOrg(adminOrgs: readonly string[] | undefined, projectOrgs: readonly string[]): string | undefined {
  return adminOrgs?.[0] ?? projectOrgs[0];
}
