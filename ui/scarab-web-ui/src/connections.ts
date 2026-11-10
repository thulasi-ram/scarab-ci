// Pure rules behind the Connections section (git-bug 19b0a45): a forge
// connection serves one org, and the org is the forge account that owns the
// repositories it binds. The server enforces every rule here; these only
// decide what the page offers, so the no-DOM vitest tier covers them.

/** What these rules read from a connection. */
export interface ConnectionLike {
  org?: string | null;
}

/** A repository as the forge lists it. */
export interface RepoLike {
  owner: string;
}

/**
 * The connections that serve no org yet. Only a global admin is sent them,
 * and only a global admin can act on them.
 */
export function unassignedConnections<T extends ConnectionLike>(conns: readonly T[]): T[] {
  return conns.filter((c) => c.org == null);
}

/**
 * The repositories the Add repository dialog offers: those owned by the
 * connection's org. The server refuses any other owner, so offering one would
 * only produce an error. `hidden` counts the rest, so the dialog can say why
 * the list is shorter than what the token reaches. An unassigned connection
 * (`org` null) is never offered the dialog, so it gets the whole list.
 */
export function reposForOrg<T extends RepoLike>(
  repos: readonly T[],
  org: string | null | undefined,
): { offered: T[]; hidden: number } {
  if (org == null) return { offered: [...repos], hidden: 0 };
  const offered = repos.filter((r) => r.owner === org);
  return { offered, hidden: repos.length - offered.length };
}

/** What a re-sync answered, as one line under the connection. */
export function resyncNote(r: { bound: string[]; confirmed: number; skipped?: string[] }): string {
  const repos = (n: number) => (n === 1 ? "repository" : "repositories");
  const head =
    r.bound.length > 0
      ? `Bound ${r.bound.length} new ${repos(r.bound.length)}: ${r.bound.join(", ")}.`
      : `Already in sync: ${r.confirmed} ${repos(r.confirmed)} confirmed.`;
  const skipped = r.skipped ?? [];
  if (skipped.length === 0) return head;
  return `${head} Not bound: ${skipped.join(", ")} (owned by another account, or already bound through another connection).`;
}

/**
 * The org an unassigned connection can be assigned to without a refusal: the
 * one owner of every repository bound through it. Null when it has none
 * bound (any org may be typed) or repositories of several owners (the server
 * refuses every org until they are moved).
 */
export function soleOwner(projects: readonly RepoLike[]): string | null {
  const owners = new Set(projects.map((p) => p.owner));
  return owners.size === 1 ? [...owners][0]! : null;
}

/**
 * Why `name` cannot name a new org, or null when it can. The org is the forge
 * account that owns the repositories, so it is one path segment. Names that
 * start with `_` are reserved for Scarab's own use.
 */
export function orgNameProblem(name: string): string | null {
  const n = name.trim();
  if (n === "") return "Name the organization.";
  if (n.startsWith("_")) return "Names that start with _ are reserved.";
  if (/[\s/]/.test(n)) return "Use the forge account name alone, without spaces or slashes.";
  return null;
}

/** A connection's kind as people name the forge: `GitHub`, `Forgejo`. */
export function forgeName(kind: string | null | undefined): string {
  if (kind === "github") return "GitHub";
  if (kind === "forgejo") return "Forgejo";
  return "the forge";
}

/** The connection that serves `org/repo`, from a connection list: the one
 * whose bound Projects include it. `null` when none in the list does (or the
 * caller may not list connections). */
export function connectionFor<T extends { projects: readonly { org: string; project: string }[] }>(
  conns: readonly T[],
  org: string,
  repo: string,
): T | null {
  return conns.find((c) => c.projects.some((p) => p.org === org && p.project === repo)) ?? null;
}

/** What the waiting panel reads from a connection. */
export interface WaitingLike {
  id: string;
  kind: string;
  web_url: string;
  projects: readonly RepoLike[];
}

/**
 * The GitHub App installation id of a connection the installation webhook
 * registered (`github-install-<id>`), or null for any other connection.
 */
export function installationId(c: { id: string; kind: string }): string | null {
  if (c.kind !== "github") return null;
  const m = /^github-install-(\d+)$/.exec(c.id);
  return m ? m[1]! : null;
}

/** One line of a waiting connection's facts: a label and its value. */
export interface WaitingFact {
  label: string;
  value: string;
}

/**
 * The facts the New organization page lists for a connection that serves no
 * org: its forge and host, its installation id (GitHub), and the account
 * that owns the repositories bound through it, when one does. The connection
 * list does not carry the installation's account login or account id, so
 * those are named only through bound repositories.
 */
export function waitingFacts(c: WaitingLike): WaitingFact[] {
  const out: WaitingFact[] = [
    { label: "forge", value: `${forgeName(c.kind)} · ${c.web_url.replace(/^https?:\/\//, "")}` },
  ];
  const install = installationId(c);
  if (install != null) out.push({ label: "installation", value: install });
  const owner = soleOwner(c.projects);
  if (owner != null) out.push({ label: "account", value: owner });
  const n = c.projects.length;
  out.push({
    label: "repositories",
    value: n === 0 ? "none bound" : `${n} bound`,
  });
  return out;
}

/**
 * The name the Create a new organization form starts with: the account that
 * owns every repository bound through the connection, or empty.
 */
export function newOrgPrefill(c: { projects: readonly RepoLike[] }): string {
  return soleOwner(c.projects) ?? "";
}

/**
 * Why `name` cannot name a new org made from a waiting connection, or null
 * when it can: `orgNameProblem`, and a name that is already an org's (that
 * is an assignment, which the other action makes).
 */
export function newOrgNameProblem(name: string, orgs: readonly string[]): string | null {
  const problem = orgNameProblem(name);
  if (problem != null) return problem;
  const n = name.trim();
  return orgs.includes(n) ? `${n} is already an organization. Use Assign to an organization.` : null;
}

/**
 * The org an Assign to an organization form starts with: the sole owner of
 * the connection's bound repositories when that is an org, else the first
 * org, else empty (there is none to assign to).
 */
export function assignDefault(c: { projects: readonly RepoLike[] }, orgs: readonly string[]): string {
  const owner = soleOwner(c.projects);
  if (owner != null && orgs.includes(owner)) return owner;
  return orgs[0] ?? "";
}

/** The first line of the New organization page's waiting panel, for `n`
 *  connections. */
export function waitingIntro(n: number): string {
  return n === 1
    ? "Scarab could not prove this account owns an organization of the same name, so it serves none until you choose."
    : "Scarab could not prove these accounts own organizations of the same names, so they serve none until you choose.";
}
