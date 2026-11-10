// Identity links, as the UI has to reason about them (ADR-0069). Pure — no
// DOM, no fetch — so the decisions that are easy to get subtly wrong are
// testable without rendering: which sign-in buttons to offer, which accounts
// can still be linked, whether unlinking is allowed, what a link callback's
// outcome means, how a flat binding list reads as one row per person, who
// has access to one repository, and what a forge re-sync changed.

/** A configured login provider (`GET /v1/auth/providers`), structurally. */
export type ProviderLike = { id: string; name: string; default: boolean };

/** One linked account (`GET /v1/me` → `identities`), structurally. */
export type IdentityLike = { provider: string; handle: string };

/** One role binding (`GET /v1/orgs/{org}/bindings`), structurally. The
 * person is named twice: `id`, the Principal's random id that a URL
 * carries, and `handle` (`github:alice`), what people read and type. */
export type BindingLike = {
  id: string;
  handle: string;
  role: string;
  /** Empty = org-wide. */
  project: string;
  display_name?: string | null;
  claimed?: boolean | null;
  /** `native` (granted in Scarab) or `import` (from the forge's
   * collaborators). Absent on an import's response. */
  origin?: string | null;
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
  /** The Principal's id: the row's key and the member page's URL. */
  id: string;
  /** What people read and type. */
  handle: string;
  display: string;
  /** `true` = imported from the forge, nobody has signed in as them yet. */
  pending: boolean;
  /** The org-wide role; `null` = they hold roles in repositories only. */
  orgRole: string | null;
  /** The repositories they also hold a role in, by name. Their roles there
   * are managed on each repository's Settings › Access. */
  repos: string[];
};

/** A person as the binding rows name them: who, under what name, and whether
 * anyone has signed in as them yet. */
function person(b: BindingLike) {
  return {
    id: b.id,
    handle: b.handle,
    display: b.display_name || b.handle,
    pending: b.claimed === false,
  };
}

const byName = (a: { display: string; handle: string }, b: { display: string; handle: string }) =>
  a.display.localeCompare(b.display) || a.handle.localeCompare(b.handle);

/** The org's Members list: one row per person, with their org role and the
 * repositories they hold a role in. Someone with repository roles only is a
 * member in practice, so they are listed too, with no org role. Alphabetical
 * by what is shown: an admin scanning for a person reads names. */
export function memberRows(bindings: BindingLike[]): MemberRow[] {
  const by = new Map<string, MemberRow>();
  for (const b of bindings) {
    let row = by.get(b.id);
    if (!row) {
      row = { ...person(b), orgRole: null, repos: [] };
      by.set(b.id, row);
    }
    if (!b.project) row.orgRole = b.role;
    else if (!row.repos.includes(b.project)) row.repos.push(b.project);
  }
  for (const row of by.values()) row.repos.sort((a, b) => a.localeCompare(b));
  return [...by.values()].sort(byName);
}

/** The roles an org role can be, lowest first. */
export const ORG_ROLES = ["viewer", "member", "admin", "owner"] as const;

/** A member's page in org settings, `/orgs/<org>/settings/members/<id>/manage`.
 *  The segment is the Principal's id (`p_` and 32 hex characters), never the
 *  handle: a handle can be an email, and a URL is not where personal data
 *  belongs. Encoded all the same. */
export const memberHref = (org: string, id: string) =>
  `/orgs/${encodeURIComponent(org)}/settings/members/${encodeURIComponent(id)}/manage`;

/** A repository's Settings tab, whose Access section holds its roles. */
export const repoAccessHref = (org: string, repo: string) =>
  `/${encodeURIComponent(org)}/${encodeURIComponent(repo)}?tab=settings`;

/** "N repositories", for a member's row. */
export const repoCountLabel = (n: number) => `${n} ${n === 1 ? "repository" : "repositories"}`;

export type MemberRepoRole = { repo: string; role: string; source: RoleSource };

/** The repositories Principal `id` holds a role in, with the role and where
 *  it came from, by repository name. Read-only on the member's page: each is
 *  changed on that repository's Settings › Access. */
export function memberRepoRoles(bindings: BindingLike[], id: string): MemberRepoRole[] {
  return bindings
    .filter((b) => b.id === id && b.project)
    .map((b) => ({ repo: b.project, role: b.role, source: roleSource(b.origin) }))
    .sort((a, b) => a.repo.localeCompare(b.repo));
}

/** Where a repository role came from: granted in Scarab, or seeded from the
 * forge's collaborators by a re-sync. `null` when the server did not say. */
export type RoleSource = "scarab" | "forge" | null;

export function roleSource(origin: string | null | undefined): RoleSource {
  if (origin === "native") return "scarab";
  if (origin === "import") return "forge";
  return null;
}

export type DirectAccessRow = {
  id: string;
  handle: string;
  display: string;
  pending: boolean;
  /** The role held on this repository. */
  role: string;
  source: RoleSource;
  /** Their org role too, if any: the higher of the two applies. */
  orgRole: string | null;
};

export type OrgAccessRow = {
  id: string;
  handle: string;
  display: string;
  pending: boolean;
  /** The org role that reaches this repository. */
  orgRole: string;
};

/** Who has a role on `repo`: the people bound to it directly, and the people
 * whose org role reaches it with no binding of their own here. An org role
 * applies to every repository of the org; a repository role is managed here,
 * the org role under Members. */
export function repoAccessRows(
  bindings: BindingLike[],
  repo: string,
): { direct: DirectAccessRow[]; viaOrg: OrgAccessRow[] } {
  const orgRoles = new Map<string, string>();
  for (const b of bindings) if (!b.project) orgRoles.set(b.id, b.role);
  const direct: DirectAccessRow[] = bindings
    .filter((b) => b.project === repo)
    .map((b) => ({
      ...person(b),
      role: b.role,
      source: roleSource(b.origin),
      orgRole: orgRoles.get(b.id) ?? null,
    }))
    .sort(byName);
  const here = new Set(direct.map((r) => r.id));
  const seen = new Set<string>();
  const viaOrg: OrgAccessRow[] = [];
  for (const b of bindings) {
    if (b.project || here.has(b.id) || seen.has(b.id)) continue;
    seen.add(b.id);
    viaOrg.push({ ...person(b), orgRole: b.role });
  }
  return { direct, viaOrg: viaOrg.sort(byName) };
}

export type ResyncChange = { id: string; handle: string; display: string; role: string; was?: string };

/** What a forge re-sync of `repo` changed, from the bindings listed before
 * and after it. Only rows the forge owns count: a re-sync never touches a
 * role granted in Scarab. A row whose origin is unknown is counted as the
 * forge's, so an older server still gets a result. */
export function resyncDiff(
  before: BindingLike[],
  after: BindingLike[],
  repo: string,
): { added: ResyncChange[]; changed: ResyncChange[]; removed: ResyncChange[] } {
  const forgeRows = (list: BindingLike[]) =>
    new Map(
      list
        .filter((b) => b.project === repo && b.origin !== "native")
        .map((b) => [b.id, b] as const),
    );
  const was = forgeRows(before);
  const now = forgeRows(after);
  const added: ResyncChange[] = [];
  const changed: ResyncChange[] = [];
  const removed: ResyncChange[] = [];
  for (const [id, b] of now) {
    const old = was.get(id);
    const row = { id, handle: b.handle, display: person(b).display, role: b.role };
    if (!old) added.push(row);
    else if (old.role !== b.role) changed.push({ ...row, was: old.role });
  }
  for (const [id, b] of was) {
    if (!now.has(id)) removed.push({ id, handle: b.handle, display: person(b).display, role: b.role });
  }
  return { added: added.sort(byName), changed: changed.sort(byName), removed: removed.sort(byName) };
}

/**
 * Append a picked Principal's handle to a comma-separated approver list,
 * once. The server stores the Principal it names, so picking beats typing.
 */
export function addToList(list: string, id: string): string {
  const items = list
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  if (!items.includes(id)) items.push(id);
  return items.join(", ");
}
