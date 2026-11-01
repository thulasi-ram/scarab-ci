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
  return `${head} Not bound: ${skipped.join(", ")} (owned by another account, or bound through another organization's connection).`;
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
