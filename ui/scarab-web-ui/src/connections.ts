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
