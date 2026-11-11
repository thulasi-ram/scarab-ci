// Pure rules behind the Connections section (git-bug 19b0a45): a forge
// connection serves one org, and the org is the forge account that owns the
// repositories it binds. The server enforces every rule here; these only
// decide what the page offers, so the no-DOM vitest tier covers them.
import type { Chip } from "./libraries";

/** What these rules read from a connection. */
export interface ConnectionLike {
  org?: string | null;
}

/**
 * The connections that serve no org yet. Only a global admin is sent them,
 * and only a global admin can act on them.
 */
export function unassignedConnections<T extends ConnectionLike>(conns: readonly T[]): T[] {
  return conns.filter((c) => c.org == null);
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

/** What a connection's list row and page read. */
export interface ConnectionSummaryLike {
  kind: string;
  base_url: string;
  web_url: string;
  credential_present: boolean;
  last_delivery_at?: number | null;
  managed_by_config: boolean;
}

/**
 * The host people know a connection by: the forge's web host (`github.com`,
 * `git.acme.dev`), not the API host (`api.github.com`). A URL that does not
 * parse is shown as written.
 */
export function connectionHost(c: { web_url: string; base_url: string }): string {
  const raw = c.web_url || c.base_url;
  try {
    return new URL(raw).host || raw;
  } catch {
    return raw;
  }
}

/** The address of one connection's page, under the org's Connections. */
export function connectionHref(org: string, id: string): string {
  return `/orgs/${encodeURIComponent(org)}/settings/connections/${encodeURIComponent(id)}`;
}

/**
 * The state tags of a connection, most urgent first: a credential that does
 * not resolve, no webhook delivery recorded, and managed by configuration.
 * A healthy connection that has had a delivery carries none.
 */
export function connectionChips(c: ConnectionSummaryLike): Chip[] {
  const out: Chip[] = [];
  if (!c.credential_present) {
    out.push({
      label: "credential missing",
      tone: "danger",
      title: "The credential's handle does not resolve, so Scarab cannot reach the forge.",
    });
  }
  if (c.last_delivery_at == null) {
    out.push({
      label: "no deliveries yet",
      tone: "muted",
      title: `No webhook delivery from ${forgeName(c.kind)} has been recorded yet.`,
    });
  }
  if (c.managed_by_config) {
    out.push({
      label: "managed by configuration",
      tone: "muted",
      title:
        "Set in the server's connections: configuration (Helm scarab.connections or SCARAB_CONNECTIONS). Change or remove it there.",
    });
  }
  return out;
}

/** "1 enabled repository", "7 enabled repositories". */
export function enabledReposLabel(n: number): string {
  return `${n} enabled ${n === 1 ? "repository" : "repositories"}`;
}

/** What the waiting panel reads from a connection. */
export interface WaitingLike {
  kind: string;
  web_url: string;
  enabled_repos: number;
  installation_id?: number | null;
  account_id?: number | null;
  account_login?: string | null;
}

/** One line of a waiting connection's facts: a label and its value. */
export interface WaitingFact {
  label: string;
  value: string;
}

/**
 * The facts the New organization page lists for a connection that serves no
 * org: its forge and host, the forge account (its login and immutable id, as
 * the last installation delivery recorded them), its installation id
 * (GitHub), and how many repositories are enabled through it. Two waiting
 * connections of different accounts differ here even with nothing enabled.
 */
export function waitingFacts(c: WaitingLike): WaitingFact[] {
  const out: WaitingFact[] = [
    { label: "forge", value: `${forgeName(c.kind)} · ${c.web_url.replace(/^https?:\/\//, "")}` },
  ];
  const account = accountLabel(c);
  if (account != null) out.push({ label: "account", value: account });
  if (c.installation_id != null) out.push({ label: "installation", value: String(c.installation_id) });
  const n = c.enabled_repos;
  out.push({
    label: "repositories",
    value: n === 0 ? "none enabled" : `${n} enabled`,
  });
  return out;
}

/** `acme (id 4321)`, the id alone while no login is recorded, or null when
 *  neither is known. */
export function accountLabel(c: Pick<WaitingLike, "account_id" | "account_login">): string | null {
  const login = c.account_login?.trim() || null;
  const id = c.account_id ?? null;
  if (login != null && id != null) return `${login} (id ${id})`;
  if (login != null) return login;
  return id != null ? `id ${id}` : null;
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

/** The first line of the New organization page's waiting panel, for `n`
 *  connections. */
export function waitingIntro(n: number): string {
  return n === 1
    ? "Scarab could not prove this account owns an organization of the same name, so it serves none until you choose."
    : "Scarab could not prove these accounts own organizations of the same names, so they serve none until you choose.";
}
