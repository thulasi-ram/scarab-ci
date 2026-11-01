// The settings route table (git-bug 734ffa1): which sections exist, where
// each one lives, who sees which group, and where the old URLs go.
//
// Two groups. **Your account** is everyone's and lives under `/settings/…`.
// **Organization settings** belongs to one org and lives under
// `/orgs/<org>/settings/…`, so an admin of two orgs can reach both and a link
// names the org it is about. The server enforces `Administer` on every
// request; the gating here only decides what to show.
//
// Pure functions over `{ can_administer, admin_orgs }`, so the no-DOM vitest
// tier covers the table, the gating and the redirects.

export type SettingsGroupKind = "account" | "org";

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
}

export const ACCOUNT_SECTIONS: readonly SettingsSection[] = [
  { id: "profile", label: "Profile", sub: "your sign-in accounts" },
  { id: "security", label: "Security", sub: "your passkeys and security events" },
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
  { id: "tokens", label: "API tokens", sub: "machine credentials" },
  { id: "security", label: "Security", sub: "step-up posture and recent refusals" },
  { id: "events", label: "Event log", sub: "every security event, filterable" },
];

export const accountHref = (section: string) => `/settings/${section}`;

export const orgHref = (org: string, section: string) =>
  `/orgs/${encodeURIComponent(org)}/settings/${section}`;

/** The org from a route's `:org` param, which the router leaves
 *  percent-encoded; a malformed escape stays as written. */
export function orgFromParam(raw: string): string {
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
  return (kind === "account" ? ACCOUNT_SECTIONS : ORG_SECTIONS).find((s) => s.id === id);
}

/**
 * The href of the sidebar section `pathname` sits in: the section's own URL,
 * or the section a page under it belongs to (`…/environments/<name>/edit` is
 * in Environments, `…/libraries/review` in Libraries). The sidebar link and
 * the narrow-screen `<select>` both mark this one. `null` outside settings or
 * for a section that does not exist.
 *
 * The org is decoded and escaped again with `orgHref`, so the result
 * compares equal to the hrefs `navGroups` builds however the path spelled it.
 */
export function activeSectionHref(pathname: string): string | null {
  const parts = pathname.split("/").filter((p) => p !== "");
  if (parts[0] === "settings" && parts[1] !== undefined) {
    return resolveSection("account", parts[1]) ? accountHref(parts[1]) : null;
  }
  if (parts[0] === "orgs" && parts[1] !== undefined && parts[2] === "settings" && parts[3] !== undefined) {
    return resolveSection("org", parts[3]) ? orgHref(orgFromParam(parts[1]), parts[3]) : null;
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

export interface NavGroup {
  kind: SettingsGroupKind;
  heading: string;
  /** The org this group configures (org group only). */
  org?: string;
  /** Other orgs the caller administers, to switch to (org group only). */
  otherOrgs?: string[];
  items: NavItem[];
}

/**
 * The sidebar for `who`, with `org` the org in the URL (undefined on an
 * account page, where the org group shows the first administrable org).
 */
export function navGroups(who: SettingsCaller, org: string | undefined): NavGroup[] {
  const groups: NavGroup[] = [
    {
      kind: "account",
      heading: "Your account",
      items: ACCOUNT_SECTIONS.map((s) => ({ id: s.id, label: s.label, href: accountHref(s.id) })),
    },
  ];
  const shown = org ?? who.admin_orgs[0];
  if (administers(who, shown)) {
    groups.push({
      kind: "org",
      heading: "Organization settings",
      org: shown,
      otherOrgs: who.admin_orgs.filter((o) => o !== shown),
      items: ORG_SECTIONS.map((s) => ({ id: s.id, label: s.label, href: orgHref(shown!, s.id) })),
    });
  }
  return groups;
}

/** Where `/settings` lands: the first org's first section, else your profile. */
export function settingsHome(who: SettingsCaller): string {
  const org = who.can_administer ? who.admin_orgs[0] : undefined;
  return org ? orgHref(org, ORG_SECTIONS[0]!.id) : accountHref(ACCOUNT_SECTIONS[0]!.id);
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
      return org ? `${orgHref(org, "libraries")}/review${search}` : settingsHome(who);
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
  return out;
}
