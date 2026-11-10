// Issued API tokens, as the UI has to reason about them (ADR-0049). Pure — no
// DOM, no fetch — so the parts that are easy to get subtly wrong (which state a
// record is in, what its role actually promises, whether a lifetime is
// mintable) are testable without rendering anything.
//
// The record the API returns carries three independent facts about a token's
// life — `revoked_at`, `expires_at`, and neither — and the panel has to collapse
// them into one word per row. Doing that inline at the call site is how "expired
// three weeks ago" ends up rendered next to a Revoke button.
import type { Chip } from "./libraries";

/** The record shape these helpers need — structural, so `ApiToken` satisfies it
 * without this module importing the generated schema. */
export type TokenLike = {
  expires_at: number;
  revoked_at?: number | null;
  created_at: number;
};

/**
 * What a token is right now.
 *
 * **Revocation outranks expiry**, deliberately. A revoked token that has since
 * sailed past its expiry is still, to an operator, the one somebody killed —
 * and the server treats the two very differently on the authentication path
 * (revocation is permanent and effective on the next request; expiry is just
 * the clock arriving). Reporting the later of the two events would rewrite the
 * moment the credential actually died.
 */
export type TokenState = "live" | "expired" | "revoked";

export function tokenState(t: TokenLike, now: number): TokenState {
  if (t.revoked_at != null) return "revoked";
  return t.expires_at <= now ? "expired" : "live";
}

/** Can this token still authenticate a request? The only state with a Revoke
 * button, and the only one worth warning about. */
export function isLive(t: TokenLike, now: number): boolean {
  return tokenState(t, now) === "live";
}

const DAY_MS = 86_400_000;

/**
 * Whole days until expiry, negative once past. Rounded DOWN so "expires in 1
 * day" never means "in twenty minutes" — a credential's remaining life is the
 * one number nobody should have to round up in their head.
 */
export function daysLeft(t: TokenLike, now: number): number {
  return Math.floor((t.expires_at - now) / DAY_MS);
}

/**
 * Should a live token's expiry be called out as imminent? Two weeks is the
 * window in which "renew this before it breaks your CI" is actionable rather
 * than noise.
 */
export const EXPIRY_WARNING_DAYS = 14;

export function expiresSoon(t: TokenLike, now: number): boolean {
  return isLive(t, now) && daysLeft(t, now) <= EXPIRY_WARNING_DAYS;
}

/** Expiry in words, for the fact line. Live tokens look forward; dead ones
 * report when the clock ran out. */
export function expiryLabel(t: TokenLike, now: number): string {
  const d = daysLeft(t, now);
  if (d < 0) return `expired ${-d === 1 ? "1 day" : `${-d} days`} ago`;
  if (d === 0) return "expires today";
  return `expires in ${d === 1 ? "1 day" : `${d} days`}`;
}

/**
 * List order: everything still usable first, newest mint at the top — so the
 * token you just minted is where you look for it — then the dead ones, most
 * recently dead first.
 *
 * Sorting purely by `created_at` would bury a fresh token under a wall of
 * revoked ones the moment an org has any history, and sorting live tokens by
 * soonest expiry would move a row out from under the cursor every time the list
 * refetches.
 */
export function sortTokens<T extends TokenLike>(tokens: readonly T[], now: number): T[] {
  const dead = (t: T) => (isLive(t, now) ? 0 : 1);
  const diedAt = (t: T) => t.revoked_at ?? t.expires_at;
  return [...tokens].sort(
    (a, b) => dead(a) - dead(b) || (dead(a) ? diedAt(b) - diedAt(a) : b.created_at - a.created_at),
  );
}

/** The longest lifetime the server will mint (`MAX_API_TOKEN_DAYS`). Mirrored
 * here so the form can refuse locally with the same number the 400 would quote;
 * the server remains the one that enforces it. */
export const MAX_TOKEN_DAYS = 365;

/**
 * Why this lifetime cannot be minted, or `null` if it can.
 *
 * There is no "never expires" and no default, on purpose: `values.yaml` already
 * records what a credential with no verb and no expiry cost this repo once, and
 * the server makes both fields required so that shape cannot be minted twice.
 */
export function lifetimeError(days: number): string | null {
  if (!Number.isInteger(days)) return "Lifetime must be a whole number of days.";
  if (days < 1) return "A token must expire — there is no unlimited lifetime.";
  if (days > MAX_TOKEN_DAYS) return `The longest a token may live is ${MAX_TOKEN_DAYS} days.`;
  return null;
}

/**
 * The roles a token's ceiling may be set to, least first.
 *
 * `what` describes the CEILING, which is not the same as what the token can do:
 * every request re-derives the owner's live role and takes the lower of the
 * two. The picker says "up to" for that reason.
 */
export const ROLE_CHOICES = [
  {
    role: "viewer",
    what: "read runs, logs and status",
  },
  {
    role: "member",
    what: "also start and re-run pipelines",
  },
  {
    role: "admin",
    what: "also change settings, secrets and bindings",
  },
  {
    role: "owner",
    what: "everything an owner of this scope can do",
  },
] as const;

export type TokenRole = (typeof ROLE_CHOICES)[number]["role"];

/** How a scope reads in a row: the org, or `org/project` when narrowed. */
export function scopeLabel(t: { org: string; project?: string | null }): string {
  return t.project ? `${t.org}/${t.project}` : t.org;
}

/** The orgs a person may be offered when minting a token for themselves:
 * those `/v1/me` lists in `token_mint_orgs` (where they hold a role), each
 * once, sorted. The server still checks the role in the exact scope. */
export function mintableOrgs(tokenMintOrgs: readonly string[]): string[] {
  return [...new Set(tokenMintOrgs)].filter((o) => o !== "").sort();
}

/** The orders the org's token list offers, in the server's `sort` values. */
export const TOKEN_SORTS = [
  { sort: "last_used", label: "Last used" },
  { sort: "use_count", label: "Calls" },
  { sort: "created", label: "Created" },
] as const;

/** How many of `tokens` can still authenticate: the count beside a list's
 * heading. */
export function activeCount(tokens: readonly TokenLike[], now: number): number {
  return tokens.filter((t) => isLive(t, now)).length;
}

/**
 * A token's state tags: what it is (active, revoked or expired), then whether
 * a live one expires within two weeks, then whether it has never been used.
 */
export function tokenChips(t: TokenLike & { last_used_at?: number | null }, now: number): Chip[] {
  const out: Chip[] = [];
  const state = tokenState(t, now);
  if (state === "revoked") {
    out.push({ label: "revoked", tone: "danger", title: "Revoked: every request that presents it is refused." });
  } else if (state === "expired") {
    out.push({ label: "expired", tone: "muted", title: "Past its expiry: every request that presents it is refused." });
  } else {
    out.push({ label: "active", tone: "ok", title: "It can authenticate requests until it expires or is revoked." });
    if (expiresSoon(t, now)) {
      out.push({
        label: "expires soon",
        tone: "motion",
        title: `It expires within ${EXPIRY_WARNING_DAYS} days. Mint a replacement before then.`,
      });
    }
  }
  if (t.last_used_at == null) {
    out.push({ label: "never used", tone: "muted", title: "No request has presented it yet." });
  }
  return out;
}
