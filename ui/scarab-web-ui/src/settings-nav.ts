// The settings route table (git-bug 734ffa1): which sections exist, where
// each one lives, who sees which group, and where the old URLs go.
//
// Three settings pages, each with its own sidebar. **Account settings** is
// everyone's and lives under `/settings/…`. **Organization settings** belongs
// to one org and lives under `/orgs/<org>/settings/…`, so an admin of two orgs
// can reach both (an org dropdown tops its sidebar) and a link names the org
// it is about. **Server administration** is for global admins
// only and lives under `/settings/server/…`: today it holds Connect a forge,
// the way a new org comes into being (its first connection), which no org's
// own settings can offer because the org does not exist yet (git-bug
// 19b0a45). The server enforces `Administer` on every request; the gating
// here only decides what to show.
//
// Pure functions over `{ can_administer, admin_orgs, global_admin }`, so the
// no-DOM vitest tier covers the table, the gating, the org dropdown, the user
// menu entries and the redirects.

export type SettingsGroupKind = "account" | "org" | "server";

export interface SettingsSection {
  id: string;
  label: string;
  /** One line for the command palette. */
  sub: string;
}

/** What the gating reads from `GET /v1/me`. */
export interface SettingsCaller {
  can_administer: boolean;
  admin_orgs: string[];
  /** Holds a global `Administer` role: may create a new org's first connection. */
  global_admin?: boolean;
}

export const ACCOUNT_SECTIONS: readonly SettingsSection[] = [
  { id: "profile", label: "Profile", sub: "your sign-in accounts" },
  { id: "security", label: "Security", sub: "your passkeys and security events" },
  { id: "tokens", label: "API tokens", sub: "your tokens for the CLI and scripts" },
  { id: "appearance", label: "Appearance", sub: "palette and light or dark mode" },
];

// Connections first: without one there are no repositories, so nothing below
// it has anything to act on.
export const ORG_SECTIONS: readonly SettingsSection[] = [
  { id: "connections", label: "Connections", sub: "the forges this org is connected to" },
  { id: "members", label: "Members", sub: "who holds which role" },
  { id: "environments", label: "Environments", sub: "where deploys land" },
  { id: "plugins", label: "Plugins", sub: "installed plugins" },
  { id: "libraries", label: "Libraries", sub: "installed libraries" },
  { id: "secrets", label: "Secrets", sub: "org-wide secrets" },
  { id: "policies", label: "Policies", sub: "CEL policies, their decisions, and every typed setting" },
  { id: "tokens", label: "API tokens", sub: "every token issued in the org" },
  { id: "security", label: "Security", sub: "step-up settings and recent refusals" },
  { id: "events", label: "Event log", sub: "every security event, filterable" },
];

/** An org's home section: where `/orgs/<org>/settings` opens, and where
 *  switching org lands when no section is in view. */
export const FIRST_ORG_SECTION = ORG_SECTIONS[0]!.id;

// Global admins only.
export const SERVER_SECTIONS: readonly SettingsSection[] = [
  {
    id: "connect-forge",
    label: "Connect a forge",
    sub: "a new organization's first forge connection",
  },
];

export const accountHref = (section: string) => `/settings/${section}`;

export const serverHref = (section: string) => `/settings/server/${section}`;

export const orgHref = (org: string, section: string) =>
  `/orgs/${encodeURIComponent(org)}/settings/${section}`;

/** The library review page of `org`, without a query. */
export const libraryReviewHref = (org: string) => `${orgHref(org, "libraries")}/review`;

/** A route param (`:org`, `:env`), which the router leaves percent-encoded,
 *  decoded; a malformed escape stays as written. */
export function decodeParam(raw: string): string {
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

export function resolveSection(
  kind: SettingsGroupKind,
  id: string | undefined,
): SettingsSection | undefined {
  const table = kind === "account" ? ACCOUNT_SECTIONS : kind === "org" ? ORG_SECTIONS : SERVER_SECTIONS;
  return table.find((s) => s.id === id);
}

/**
 * The href of the sidebar section `pathname` sits in: the section's own URL,
 * or the section a page under it belongs to (`…/environments/<name>/edit` is
 * in Environments, `…/libraries/review` in Libraries). The sidebar link and
 * the narrow-screen `<select>` both mark this one. `null` outside settings or
 * for a section that does not exist.
 *
 * The org is decoded and escaped again with `orgHref`, so the result
 * compares equal to the hrefs `navGroup` builds however the path spelled it.
 */
export function activeSectionHref(pathname: string): string | null {
  const parts = pathname.split("/").filter((p) => p !== "");
  if (parts[0] === "settings" && parts[1] === "server" && parts[2] !== undefined) {
    return resolveSection("server", parts[2]) ? serverHref(parts[2]) : null;
  }
  if (parts[0] === "settings" && parts[1] !== undefined) {
    return resolveSection("account", parts[1]) ? accountHref(parts[1]) : null;
  }
  if (parts[0] === "orgs" && parts[1] !== undefined && parts[2] === "settings" && parts[3] !== undefined) {
    return resolveSection("org", parts[3]) ? orgHref(decodeParam(parts[1]), parts[3]) : null;
  }
  return null;
}

/** May `who` administer `org`? Presentation only; the server decides. */
export const administers = (who: SettingsCaller | undefined, org: string | undefined) =>
  !!who && !!org && who.can_administer && who.admin_orgs.includes(org);

export interface NavItem {
  id: string;
  label: string;
  href: string;
}

/** One entry of the org dropdown. */
export interface OrgChoice {
  org: string;
  /** False only for the org in the URL when the caller does not administer
   *  it: the dropdown shows it, but it cannot be picked. */
  administered: boolean;
}

/** The sidebar of one settings page: one group, never the others. */
export interface NavGroup {
  kind: SettingsGroupKind;
  heading: string;
  /** The org this page configures (org page only). */
  org?: string;
  /** The org dropdown's entries (org page only). Empty when the caller
   *  administers no org, and then no dropdown is shown. */
  orgs?: OrgChoice[];
  items: NavItem[];
}

export const GROUP_HEADINGS: Record<SettingsGroupKind, string> = {
  account: "Account settings",
  org: "Organization settings",
  server: "Server administration",
};

/** The orgs `who` administers, sorted. */
export function administeredOrgs(who: SettingsCaller): string[] {
  return who.can_administer ? [...who.admin_orgs].sort() : [];
}

/**
 * The org dropdown at the top of an org page's sidebar: every org `who`
 * administers, shown even when there is only one so the control is always in
 * the same place. When the org in the URL is not one of them, it is listed
 * too, marked not administered, so the dropdown still names the page's org.
 */
export function orgChoices(who: SettingsCaller, current: string): OrgChoice[] {
  const orgs = administeredOrgs(who);
  if (orgs.length === 0) return [];
  const out: OrgChoice[] = orgs.map((org) => ({ org, administered: true }));
  if (!orgs.includes(current)) out.unshift({ org: current, administered: false });
  return out;
}

/** Where picking `target` in the org dropdown goes: the same section in that
 *  org, or its first section when no known section is in view. */
export function orgSwitchHref(target: string, section: string | undefined): string {
  return resolveSection("org", section) ? orgHref(target, section!) : groupHome("org", target);
}

/**
 * The sidebar of the settings page of `kind` (with `org` the org in the URL
 * on an org page): that page's sections only. Account settings, an org's
 * settings and server administration are separate pages; the user menu
 * links each one (`settingsMenu`).
 *
 * The sections are listed only to a caller who may use them: an org page
 * lists them to an admin of that org, the server page to a global admin.
 */
export function navGroup(who: SettingsCaller, kind: SettingsGroupKind, org?: string): NavGroup {
  const heading = GROUP_HEADINGS[kind];
  if (kind === "org") {
    const shown = org ?? "";
    return {
      kind,
      heading,
      org: shown,
      orgs: orgChoices(who, shown),
      items: administers(who, shown)
        ? ORG_SECTIONS.map((s) => ({ id: s.id, label: s.label, href: orgHref(shown, s.id) }))
        : [],
    };
  }
  if (kind === "server") {
    return {
      kind,
      heading,
      items: who.global_admin
        ? SERVER_SECTIONS.map((s) => ({ id: s.id, label: s.label, href: serverHref(s.id) }))
        : [],
    };
  }
  return {
    kind,
    heading,
    items: ACCOUNT_SECTIONS.map((s) => ({ id: s.id, label: s.label, href: accountHref(s.id) })),
  };
}

/** Top-level paths that are not an org (`/:org/:repo` sits below them). */
const NOT_AN_ORG = new Set(["settings", "orgs", "account", "security"]);

/**
 * The org `pathname` is about, or undefined: `/<org>/<repo>/…` and
 * `/orgs/<org>/settings/…`. Decoded.
 */
export function contextOrg(pathname: string): string | undefined {
  const parts = pathname.split("/").filter((p) => p !== "");
  if (parts[0] === "orgs" && parts[1] !== undefined && parts[2] === "settings") return decodeParam(parts[1]);
  if (parts.length >= 2 && !NOT_AN_ORG.has(parts[0]!)) return decodeParam(parts[0]!);
  return undefined;
}

export interface MenuEntry {
  kind: SettingsGroupKind;
  label: string;
  /** One line for the command palette. */
  sub: string;
  href: string;
}

/**
 * The settings entries of the user menu, one per settings page `who` can
 * use. Organization settings opens the org of the page in view when the
 * caller administers it (`pathname`, see `contextOrg`), else their first
 * administered org; it is left out for a caller who administers none. Server
 * administration is for global admins only.
 */
export function settingsMenu(who: SettingsCaller, pathname: string): MenuEntry[] {
  const out: MenuEntry[] = [
    {
      kind: "account",
      label: GROUP_HEADINGS.account,
      sub: "your profile, security and API tokens",
      href: groupHome("account"),
    },
  ];
  const orgs = administeredOrgs(who);
  const here = contextOrg(pathname);
  const org = here !== undefined && orgs.includes(here) ? here : orgs[0];
  if (org !== undefined) {
    out.push({ kind: "org", label: GROUP_HEADINGS.org, sub: org, href: groupHome("org", org) });
  }
  if (who.global_admin) {
    out.push({ kind: "server", label: GROUP_HEADINGS.server, sub: "this server", href: groupHome("server") });
  }
  return out;
}

/** Where a group's URL with no section opens: its first section
 *  (`/orgs/<org>/settings`, `/settings/server`). */
export function groupHome(kind: SettingsGroupKind, org?: string): string {
  if (kind === "org" && org !== undefined) return orgHref(org, FIRST_ORG_SECTION);
  if (kind === "server") return serverHref(SERVER_SECTIONS[0]!.id);
  return accountHref(ACCOUNT_SECTIONS[0]!.id);
}

/**
 * Where `/settings` lands: the first org's first section; for a global admin
 * with no org yet, Connect a forge (the only way an org comes to exist); else
 * your profile.
 */
export function settingsHome(who: SettingsCaller): string {
  const org = who.can_administer ? who.admin_orgs[0] : undefined;
  if (org) return groupHome("org", org);
  if (who.global_admin) return groupHome("server");
  return groupHome("account");
}

/**
 * Where an old settings URL now lives, keeping its query string (an old
 * `/account?identity_linked=…` or `/account/security?enroll=1` link still
 * lands on the right section). `null` when `pathname` is not an old URL.
 */
export function legacyRedirect(pathname: string, search: string, who: SettingsCaller): string | null {
  const path = pathname.replace(/\/+$/, "") || "/";
  switch (path) {
    case "/account":
      return accountHref("profile") + search;
    case "/account/security":
      return accountHref("security") + search;
    case "/settings":
      return settingsHome(who) + search;
    case "/settings/libraries/review": {
      const org = who.admin_orgs[0];
      return org ? `${libraryReviewHref(org)}${search}` : settingsHome(who);
    }
    default:
      return null;
  }
}

export interface PaletteEntry {
  label: string;
  sub: string;
  href: string;
}

/** Every settings section `who` can reach, for the ⌘K palette. */
export function paletteEntries(who: SettingsCaller): PaletteEntry[] {
  const out: PaletteEntry[] = ACCOUNT_SECTIONS.map((s) => ({
    label: `${s.label} · your account`,
    sub: s.sub,
    href: accountHref(s.id),
  }));
  for (const org of who.admin_orgs.filter((o) => administers(who, o))) {
    for (const s of ORG_SECTIONS) {
      out.push({ label: `${s.label} · ${org}`, sub: s.sub, href: orgHref(org, s.id) });
    }
  }
  if (who.global_admin) {
    for (const s of SERVER_SECTIONS) {
      out.push({ label: `${s.label} · server`, sub: s.sub, href: serverHref(s.id) });
    }
  }
  return out;
}
