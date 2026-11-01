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
  { id: "security", label: "Security", sub: "step-up and the security event log" },
];

export const accountHref = (section: string) => `/settings/${section}`;

export const orgHref = (org: string, section: string) =>
  `/orgs/${encodeURIComponent(org)}/settings/${section}`;

export function resolveSection(
  kind: SettingsGroupKind,
  id: string | undefined,
): SettingsSection | undefined {
  return (kind === "account" ? ACCOUNT_SECTIONS : ORG_SECTIONS).find((s) => s.id === id);
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
 * Where an old settings URL now lives, keeping its query string (the server
 * still sends people to `/account?identity_linked=…` and
 * `/account/security?enroll=1`). `null` when `pathname` is not an old URL.
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
