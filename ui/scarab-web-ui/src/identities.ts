// Identity links, as the UI has to reason about them (ADR-0069). Pure — no
// DOM, no fetch — so the decisions that are easy to get subtly wrong are
// testable without rendering: which sign-in buttons to offer, which accounts
// can still be linked, whether unlinking is allowed, what a link callback's
// outcome means, and how a flat binding list reads as one row per person.

/** A configured login provider (`GET /v1/auth/providers`), structurally. */
export type ProviderLike = { id: string; name: string; default: boolean };

/** One linked account (`GET /v1/me` → `identities`), structurally. */
export type IdentityLike = { provider: string; handle: string };

/** One role binding (`GET /v1/orgs/{org}/bindings`), structurally. */
export type BindingLike = {
  subject: string;
  role: string;
  /** Empty = org-wide. */
  project: string;
  display_name?: string | null;
  claimed?: boolean | null;
};

export type LoginChoice = { href: string; label: string; primary: boolean };

/**
 * The sign-in buttons. One provider (or none known — the providers list is
 * best-effort) keeps the single provider-agnostic "Sign in" the page always
 * had: ADR-0049 never tells the browser who is at the other end. Several get a
 * button each, the default one first and primary — the one an unnamed login
 * would have used anyway.
 */
export function loginChoices(providers: ProviderLike[], next?: string): LoginChoice[] {
  // Where to land after the callback (ADR-0073): a deep link — a step-up
  // confirmation link above all — must survive the sign-in it triggered.
  // Only a relative path the server would accept; `/` is the default anyway.
  const land = next && next !== "/" && safeNext(next) ? `next=${encodeURIComponent(next)}` : "";
  const href = (provider?: string) => {
    const q = [provider ? `provider=${encodeURIComponent(provider)}` : "", land].filter(Boolean);
    return q.length ? `/v1/auth/login?${q.join("&")}` : "/v1/auth/login";
  };
  if (providers.length <= 1) {
    const only = providers[0];
    return [
      {
        href: href(only?.id),
        label: "Sign in",
        primary: true,
      },
    ];
  }
  return [...providers]
    .sort((a, b) => Number(b.default) - Number(a.default))
    .map((p, i) => ({
      href: href(p.id),
      label: `Sign in with ${p.name}`,
      primary: i === 0,
    }));
}

/** The longest `next` the server accepts (`oauth::MAX_NEXT_LEN`, bytes). */
export const MAX_NEXT_LEN = 512;

/**
 * The server's `safe_next`, mirrored: a relative path starting with `/`, not
 * `//`, with no `\` and no control character, at most 512 bytes. The server
 * answers 400 to anything else — so a `next` that fails here is dropped
 * rather than sent, and the sign-in still works.
 */
export function safeNext(next: string): boolean {
  return (
    next.startsWith("/") &&
    !next.startsWith("//") &&
    !next.includes("\\") &&
    !/[\u0000-\u001f\u007f-\u009f]/.test(next) &&
    new TextEncoder().encode(next).length <= MAX_NEXT_LEN
  );
}

/**
 * Providers the person has no account linked on yet — the Link buttons. A
 * Principal holds at most one identity per provider (the server refuses a
 * second), so a linked provider is never offered again.
 */
export function linkable(providers: ProviderLike[], identities: IdentityLike[]): ProviderLike[] {
  const held = new Set(identities.map((i) => i.provider));
  return providers.filter((p) => !held.has(p.id));
}

/** May an identity be unlinked? Never the last one: the server refuses, and a
 * Principal nobody can sign in as is a locked door. */
export function canUnlink(identities: IdentityLike[]): boolean {
  return identities.length > 1;
}

/** A provider's display name, falling back to its id for one no longer
 * configured (an identity outlives a config change). */
export function providerName(providers: ProviderLike[], id: string): string {
  return providers.find((p) => p.id === id)?.name ?? id;
}

export type Notice = { kind: "ok" | "error"; text: string };

/**
 * What the link callback's redirect (`/account?…`) says happened. The
 * callback is a browser redirect, so its outcome rides in the query string:
 * `identity_linked` (+ `merged` when another Principal was folded in), or
 * `identity_link_error` with the server's own explanation.
 */
export function linkNotice(search: string, providers: ProviderLike[]): Notice | null {
  const q = new URLSearchParams(search);
  const error = q.get("identity_link_error");
  if (error) return { kind: "error", text: error };
  const linked = q.get("identity_linked");
  if (!linked) return null;
  const name = providerName(providers, linked);
  const merged = q.get("merged");
  return {
    kind: "ok",
    text: merged
      ? `Linked your ${name} account. It had signed in before as ${merged}; that history, its roles and its tokens are now yours.`
      : `Linked your ${name} account. You can now sign in with it.`,
  };
}

/** The merge the link callback asks the person to confirm (`identity_merge`),
 * or `null`. Set when the linked sign-in is the only one of an account that
 * holds passkeys: one of that account's passkeys must confirm the merge. */
export function pendingMergeId(search: string): string | null {
  return new URLSearchParams(search).get("identity_merge") || null;
}

/** What a confirmed merge did, in words. `merged` is `null` when the
 * sign-in was linked without a merge: the other account let go of it after
 * the link started. */
export function mergedNotice(merged: string | null, removedPasskeys: number): Notice {
  if (merged === null) {
    return {
      kind: "ok",
      text: "Linked the account. It no longer belonged to another account, so nothing was merged.",
    };
  }
  const keys =
    removedPasskeys === 1 ? "Its passkey was deleted" : `Its ${removedPasskeys} passkeys were deleted`;
  return {
    kind: "ok",
    text: `Merged ${merged} into your account: its history, roles and tokens are now yours. ${keys}; add a passkey of yours under Security if you want to use that device here.`,
  };
}

/** The steps of confirming a merge, injected so the flow is testable. */
export type MergeConfirmDeps = {
  /** Sign the challenge with a passkey (the browser prompt). */
  assert: (publicKey: unknown) => Promise<Record<string, unknown>>;
  /** POST the assertion. */
  confirm: (
    id: string,
    credential: Record<string, unknown>,
  ) => Promise<{ merged?: string | null; removed_passkeys: number }>;
  /** Load the merge again: called after a refused POST, which has spent the
   * challenge, so the panel shows that the link must start again. */
  reload: () => unknown;
  /** A WebAuthn failure in words. */
  webauthnError: (e: unknown) => string;
};

/** Confirm merge `id`: the notice to show, or the error. A dismissed passkey
 * prompt spends nothing, so the same challenge can be tried again; a refused
 * POST has spent it, so the merge is loaded again. */
export async function confirmMerge(
  id: string,
  publicKey: unknown,
  deps: MergeConfirmDeps,
): Promise<{ notice: Notice } | { error: string }> {
  let credential: Record<string, unknown>;
  try {
    credential = await deps.assert(publicKey);
  } catch (e) {
    return { error: deps.webauthnError(e) };
  }
  try {
    const done = await deps.confirm(id, credential);
    return { notice: mergedNotice(done.merged ?? null, done.removed_passkeys) };
  } catch (e) {
    await deps.reload();
    return { error: e instanceof Error ? e.message : "The merge failed." };
  }
}

export type MemberRow = {
  subject: string;
  display: string;
  /** `false` = imported from the forge, nobody has signed in as them yet. */
  pending: boolean;
  /** The org-wide role, if any. */
  orgRole: string | null;
  /** Per-project roles, by project name. */
  projects: { project: string; role: string }[];
};

/** One row per person, alphabetical by what is shown — an admin scanning for
 * "who can deploy" reads names, not binding tuples. */
export function memberRows(bindings: BindingLike[]): MemberRow[] {
  const by = new Map<string, MemberRow>();
  for (const b of bindings) {
    let row = by.get(b.subject);
    if (!row) {
      row = {
        subject: b.subject,
        display: b.display_name || b.subject,
        pending: b.claimed === false,
        orgRole: null,
        projects: [],
      };
      by.set(b.subject, row);
    }
    if (b.project) row.projects.push({ project: b.project, role: b.role });
    else row.orgRole = b.role;
  }
  for (const row of by.values()) row.projects.sort((a, b) => a.project.localeCompare(b.project));
  return [...by.values()].sort(
    (a, b) => a.display.localeCompare(b.display) || a.subject.localeCompare(b.subject),
  );
}

/**
 * Append a picked Principal id to a comma-separated approver list, once.
 * Approvers are matched by exact id (ADR-0069 §3), so picking beats typing.
 */
export function addToList(list: string, id: string): string {
  const items = list
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  if (!items.includes(id)) items.push(id);
  return items.join(", ");
}
