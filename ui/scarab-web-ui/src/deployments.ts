// Display derivations for the Deployment record (ADR-0070), kept pure so the
// Deployments page and its tests agree on what each row says.
//
// The record's vocabulary (CONTEXT.md §4.6): a Deployment is a version made, or
// asked to be made, live on a DeploymentTarget. Two display terms are computed
// by the server and only *shown* here — never re-derived:
//   - Unexplained — no Deploy Run's request explains it; something outside
//                   Scarab changed what is live. It can still list requesters
//                   that are only *claims*: runs that named the version after
//                   the source had already shown it (ADR-0070 §2).
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
      title:
        "No Deploy Run's request explains this — it was changed outside Scarab. A run that named it afterwards is only a claim.",
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

/** Who made a Deployment live: its requesters minus the claims (a claim is a
 *  run that named the version only after its source had already shown it, so
 *  it did not make the change). The server decides which are claims. */
export function madeLiveBy(d: Pick<Deployment, "requesters">): DeploymentRequester[] {
  return d.requesters.filter((r) => !r.claim);
}

/** A source's report link, usable as an href only if it parses as an
 *  `http:`/`https:` URL; anything else (`javascript:`, `data:`, garbage) is
 *  `null`. The server already drops other schemes — this is the second lock,
 *  since sources are untrusted plugins and Solid does not sanitize hrefs. */
export function safeHref(link: string | null | undefined): string | null {
  if (!link) return null;
  try {
    const { protocol } = new URL(link);
    return protocol === "http:" || protocol === "https:" ? link : null;
  } catch {
    return null;
  }
}

/** The run page a requester points at, or `null` when its Project is unknown —
 *  a run URL needs `/{org}/{project}`, and guessing the project would link to
 *  the wrong repo's run page. */
export function requesterHref(org: string, r: Pick<DeploymentRequester, "run" | "project">): string | null {
  if (!r.project) return null;
  return `/${encodeURIComponent(org)}/${encodeURIComponent(r.project)}/runs/${encodeURIComponent(r.run)}`;
}

/** What one requester row of the Deployments page shows: the link to its run
 *  (null when the Project is unknown — shown, not linked), its text, and the
 *  hover title. The run is named by its short id: `DeploymentRequesterDto`
 *  carries no run number (`#N`), and looking one up per requester would cost
 *  a query each. The short id is the id's LAST 8 characters — a run id is a
 *  UUIDv7, whose leading characters are its timestamp, so two runs created
 *  close together share a prefix and only the random tail tells them apart. */
export function requesterRow(
  org: string,
  r: Pick<DeploymentRequester, "run" | "step" | "project">,
): { href: string | null; text: string; title: string } {
  const href = requesterHref(org, r);
  return {
    href,
    text: `${r.run.slice(-8)} · ${r.step}`,
    title: href ? `run ${r.run} in ${r.project}` : `run ${r.run} (project unknown)`,
  };
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

/** The Org the page reads: the one the URL names (`?org=`, set by every link
 *  into the page — a user in several orgs must land on the right board), else
 *  the one the caller administers, else the first org any visible Project
 *  belongs to (a member who administers nothing still reads their org's
 *  Deployments). `undefined` = no org yet. */
export function pickOrg(
  requestedOrg: string | undefined,
  adminOrgs: readonly string[] | undefined,
  projectOrgs: readonly string[],
): string | undefined {
  return requestedOrg || (adminOrgs?.[0] ?? projectOrgs[0]);
}

/** The page's own URL for one Org's Environment: what a link from elsewhere
 *  (a repo's Environments tab) points at. */
export function deploymentsHref(org: string, env?: string): string {
  const q = new URLSearchParams({ org });
  if (env) q.set("env", env);
  return `/deployments?${q.toString()}`;
}
