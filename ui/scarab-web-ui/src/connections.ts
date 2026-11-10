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
export function resyncNote(r: {
  available: string[];
  moved?: string[];
  confirmed: number;
  skipped?: string[];
}): string {
  const repos = (n: number) => (n === 1 ? "repository" : "repositories");
  const parts = [`${r.confirmed} enabled ${repos(r.confirmed)} confirmed.`];
  const moved = r.moved ?? [];
  if (moved.length > 0) {
    parts.push(`Moved ${moved.length} ${repos(moved.length)} to this connection: ${moved.join(", ")}.`);
  }
  if (r.available.length > 0) {
    parts.push(
      `${r.available.length} ${repos(r.available.length)} not enabled: ${r.available.join(", ")}. Enable them on the organization page.`,
    );
  }
  const skipped = r.skipped ?? [];
  if (skipped.length > 0) {
    parts.push(
      `Not listed: ${skipped.join(", ")} (owned by another account, or enabled through another connection).`,
    );
  }
  return parts.join(" ");
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
