import { describe, it, expect } from "vitest";
import {
  ACCOUNT_SECTIONS,
  NEW_ORG_CHOICE,
  NEW_ORG_HREF,
  ORG_SECTIONS,
  accountHref,
  activeSectionHref,
  addConnectionHref,
  contextOrg,
  groupHome,
  legacyRedirect,
  libraryReviewHref,
  navGroup,
  decodeParam,
  orgChoices,
  orgHref,
  orgPickHref,
  orgSwitchHref,
  paletteEntries,
  resolveSection,
  settingsHome,
  showsOrgPick,
  settingsMenu,
} from "./settings-nav";

type Who = { can_administer: boolean; admin_orgs: string[]; global_admin?: boolean };
const member: Who = { can_administer: false, admin_orgs: [] };
const admin: Who = { can_administer: true, admin_orgs: ["acme"] };
const twoOrgs: Who = { can_administer: true, admin_orgs: ["acme", "globex"] };
// A global admin on a fresh server: no org exists until its first connection.
const freshGlobal: Who = { can_administer: true, admin_orgs: [], global_admin: true };
const globalWithOrg: Who = { can_administer: true, admin_orgs: ["acme"], global_admin: true };

describe("route table", () => {
  it("gives every section its own URL, with no two sections sharing one", () => {
    const hrefs = [
      ...ACCOUNT_SECTIONS.map((s) => accountHref(s.id)),
      ...ORG_SECTIONS.map((s) => orgHref("acme", s.id)),
    ];
    expect(new Set(hrefs).size).toBe(hrefs.length);
    expect(accountHref("profile")).toBe("/settings/profile");
    expect(orgHref("acme", "members")).toBe("/orgs/acme/settings/members");
  });

  it("escapes the org in the path", () => {
    expect(orgHref("a b/c", "members")).toBe("/orgs/a%20b%2Fc/settings/members");
  });

  it("decodes the org from a route param, and keeps a malformed one as written", () => {
    expect(decodeParam("a%20b%2Fc")).toBe("a b/c");
    expect(decodeParam("acme")).toBe("acme");
    expect(decodeParam("bad%zz")).toBe("bad%zz");
  });

  it("resolves a known section and refuses an unknown one", () => {
    expect(resolveSection("org", "members")?.label).toBe("Members");
    expect(resolveSection("account", "security")?.label).toBe("Security");
    expect(resolveSection("account", "appearance")?.label).toBe("Appearance");
    // API tokens are personal: every member mints their own under Your
    // account, and the org section lists every token issued in the org.
    expect(resolveSection("account", "tokens")?.label).toBe("API tokens");
    expect(resolveSection("org", "tokens")?.label).toBe("API tokens");
    expect(resolveSection("org", "profile")).toBeUndefined();
    expect(resolveSection("account", "members")).toBeUndefined();
  });

  it("keeps the org's event log in its Security section, last in the org group", () => {
    expect(ORG_SECTIONS.map((s) => s.id).at(-1)).toBe("security");
    expect(navGroup(admin, "org", "acme").items.at(-1)!.href).toBe("/orgs/acme/settings/security");
    // No section of its own any more: the old URL redirects (see "old URLs").
    expect(resolveSection("org", "events")).toBeUndefined();
    expect(activeSectionHref("/orgs/acme/settings/events")).toBeNull();
    expect(resolveSection("account", "events")).toBeUndefined();
  });
});

describe("one settings page, one group", () => {
  it("lists only the account sections on an account page, for everyone", () => {
    for (const who of [member, admin, globalWithOrg]) {
      const g = navGroup(who, "account");
      expect(g.kind).toBe("account");
      expect(g.heading).toBe("Account settings");
      expect(g.items.map((i) => i.href)).toEqual([
        "/settings/profile",
        "/settings/security",
        "/settings/tokens",
        "/settings/appearance",
      ]);
      expect(g.orgs).toBeUndefined();
    }
  });

  it("lists only that org's sections on an org page", () => {
    const g = navGroup(globalWithOrg, "org", "acme");
    expect(g.heading).toBe("Organization settings");
    expect(g.org).toBe("acme");
    expect(g.items.map((i) => i.href)).toEqual(ORG_SECTIONS.map((s) => orgHref("acme", s.id)));
    // No account section rides along.
    expect(g.items.some((i) => i.href.startsWith("/settings/"))).toBe(false);
  });

  it("lists no org sections for an org the caller does not administer", () => {
    expect(navGroup(admin, "org", "globex").items).toEqual([]);
    expect(navGroup(member, "org", "acme").items).toEqual([]);
    // Listed in admin_orgs but without Administer: still nothing.
    expect(navGroup({ can_administer: false, admin_orgs: ["acme"] }, "org", "acme").items).toEqual([]);
  });
});

describe("the org dropdown", () => {
  it("is there with one org, so it is always in the same place", () => {
    expect(orgChoices(admin, "acme")).toEqual([{ org: "acme", administered: true }]);
    expect(navGroup(admin, "org", "acme").orgs).toEqual([{ org: "acme", administered: true }]);
  });

  it("lists every org the caller administers, sorted, whichever is in view", () => {
    const unsorted: Who = { can_administer: true, admin_orgs: ["globex", "acme"] };
    const want = [
      { org: "acme", administered: true },
      { org: "globex", administered: true },
    ];
    expect(orgChoices(unsorted, "acme")).toEqual(want);
    expect(orgChoices(unsorted, "globex")).toEqual(want);
  });

  it("shows the org in view, not pickable, when the caller does not administer it", () => {
    expect(orgChoices(twoOrgs, "initech")).toEqual([
      { org: "initech", administered: false },
      { org: "acme", administered: true },
      { org: "globex", administered: true },
    ]);
  });

  it("is not shown to a caller who administers no org and is not a global admin", () => {
    expect(orgChoices(member, "acme")).toEqual([]);
    expect(orgChoices({ can_administer: false, admin_orgs: ["acme"] }, "acme")).toEqual([]);
    expect(showsOrgPick(navGroup(member, "org", "acme"))).toBe(false);
    expect(showsOrgPick(navGroup(member, "org"))).toBe(false);
  });

  it("keeps the section in view when switching org", () => {
    expect(orgSwitchHref("globex", "secrets")).toBe("/orgs/globex/settings/secrets");
    // From a page under a section (one Environment), the section itself.
    expect(orgSwitchHref("globex", "environments")).toBe("/orgs/globex/settings/environments");
    expect(orgSwitchHref("a b", "members")).toBe("/orgs/a%20b/settings/members");
  });

  it("falls back to the first section when no known section is in view", () => {
    expect(orgSwitchHref("globex", undefined)).toBe("/orgs/globex/settings/connections");
    expect(orgSwitchHref("globex", "nope")).toBe("/orgs/globex/settings/connections");
    // An account section is not an org section.
    expect(orgSwitchHref("globex", "profile")).toBe("/orgs/globex/settings/connections");
  });
});

describe("New organization", () => {
  it("lives where an org's name goes, under a name no org may have", () => {
    expect(NEW_ORG_HREF).toBe("/orgs/_new");
    // `_`-prefixed names are Scarab's own; the server refuses them for an org.
    expect(NEW_ORG_HREF.split("/")[2]!.startsWith("_")).toBe(true);
    // The dropdown value holds a `/`, which no org name may.
    expect(NEW_ORG_CHOICE).toContain("/");
  });

  it("ends the org dropdown of a global admin, and no one else's", () => {
    expect(navGroup(globalWithOrg, "org", "acme").newOrg).toBe(true);
    expect(navGroup(admin, "org", "acme").newOrg).toBe(false);
    expect(navGroup(member, "org", "acme").newOrg).toBe(false);
  });

  it("is in the dropdown of a global admin with no org yet, alone", () => {
    const g = navGroup(freshGlobal, "org");
    expect(g.orgs).toEqual([]);
    expect(g.newOrg).toBe(true);
    expect(showsOrgPick(g)).toBe(true);
  });

  it("opens its page when picked, and an org pick still keeps the section", () => {
    expect(orgPickHref(NEW_ORG_CHOICE, "members")).toBe("/orgs/_new");
    expect(orgPickHref("globex", "members")).toBe("/orgs/globex/settings/members");
    // From the New organization page no section is in view.
    expect(orgPickHref("globex", undefined)).toBe("/orgs/globex/settings/connections");
  });

  it("is a page with no sections, its dropdown listing the orgs the caller administers", () => {
    const g = navGroup(twoOrgs, "org");
    expect(g.heading).toBe("Organization settings");
    expect(g.org).toBeUndefined();
    expect(g.items).toEqual([]);
    expect(g.orgs).toEqual([
      { org: "acme", administered: true },
      { org: "globex", administered: true },
    ]);
  });

  it("shows the org in view, not pickable, to a global admin who does not administer it", () => {
    expect(orgChoices(freshGlobal, "acme")).toEqual([{ org: "acme", administered: false }]);
  });

  it("is in the palette for a global admin only", () => {
    expect(paletteEntries(freshGlobal).map((e) => e.href)).toContain(NEW_ORG_HREF);
    expect(paletteEntries(globalWithOrg).map((e) => e.href)).toContain(NEW_ORG_HREF);
    expect(paletteEntries(admin).map((e) => e.href)).not.toContain(NEW_ORG_HREF);
  });

  it("is not a section, and no sidebar marks a section on it", () => {
    expect(activeSectionHref(NEW_ORG_HREF)).toBeNull();
    expect(contextOrg(NEW_ORG_HREF)).toBeUndefined();
  });
});

describe("where a forge connection is added", () => {
  it("is the first org's Connections, else New organization for a global admin", () => {
    expect(addConnectionHref(admin)).toBe("/orgs/acme/settings/connections");
    expect(addConnectionHref(globalWithOrg)).toBe("/orgs/acme/settings/connections");
    expect(addConnectionHref(freshGlobal)).toBe("/orgs/_new");
    expect(addConnectionHref(member)).toBeNull();
    expect(addConnectionHref({ can_administer: false, admin_orgs: ["acme"] })).toBeNull();
  });
});

describe("the user menu's settings entries", () => {
  const kinds = (who: Who, path = "/") => settingsMenu(who, path).map((e) => e.kind);

  it("offers a member Account settings only", () => {
    expect(settingsMenu(member, "/")).toEqual([
      expect.objectContaining({ label: "Account settings", href: "/settings/profile" }),
    ]);
    expect(kinds({ can_administer: false, admin_orgs: ["acme"] })).toEqual(["account"]);
  });

  it("offers Organization settings to an org admin and to a global admin", () => {
    expect(kinds(admin)).toEqual(["account", "org"]);
    expect(kinds(globalWithOrg)).toEqual(["account", "org"]);
    expect(settingsMenu(globalWithOrg, "/").find((e) => e.kind === "org")!.href).toBe(
      "/orgs/acme/settings/connections",
    );
    // A global admin before any org exists: the New organization page.
    expect(kinds(freshGlobal)).toEqual(["account", "org"]);
    const org = settingsMenu(freshGlobal, "/").find((e) => e.kind === "org")!;
    expect(org.label).toBe("Organization settings");
    expect(org.href).toBe("/orgs/_new");
  });

  it("opens the org in view when the caller administers it", () => {
    const org = (path: string) => settingsMenu(twoOrgs, path).find((e) => e.kind === "org")!.href;
    expect(org("/globex/shared-ci")).toBe("/orgs/globex/settings/connections");
    expect(org("/globex/shared-ci/runs/0190")).toBe("/orgs/globex/settings/connections");
    expect(org("/orgs/globex/settings/members")).toBe("/orgs/globex/settings/connections");
    expect(org("/orgs/globex/settings/environments/prod/edit")).toBe("/orgs/globex/settings/connections");
  });

  it("opens the first administered org elsewhere, or in an org the caller does not administer", () => {
    const org = (path: string) => settingsMenu(twoOrgs, path).find((e) => e.kind === "org")!.href;
    expect(org("/")).toBe("/orgs/acme/settings/connections");
    expect(org("/settings/profile")).toBe("/orgs/acme/settings/connections");
    expect(org("/orgs/_new")).toBe("/orgs/acme/settings/connections");
    expect(org("/initech/web")).toBe("/orgs/acme/settings/connections");
  });
});

describe("the org a page is about", () => {
  it("reads it from a repository path and an org settings path, decoded", () => {
    expect(contextOrg("/acme/orders-api")).toBe("acme");
    expect(contextOrg("/acme/orders-api/runs/1")).toBe("acme");
    expect(contextOrg("/acme/orders-api/run")).toBe("acme");
    expect(contextOrg("/orgs/a%20b/settings/members")).toBe("a b");
    expect(contextOrg("/orgs/acme/settings")).toBe("acme");
  });

  it("finds none on a page that is not about an org", () => {
    expect(contextOrg("/")).toBeUndefined();
    expect(contextOrg("/settings/profile")).toBeUndefined();
    expect(contextOrg("/account/security")).toBeUndefined();
    expect(contextOrg("/security/confirm/abc")).toBeUndefined();
    // One segment is not a repository path.
    expect(contextOrg("/acme")).toBeUndefined();
  });
});

describe("which section the sidebar marks", () => {
  it("marks a section on its own URL, with or without a trailing slash", () => {
    expect(activeSectionHref("/orgs/acme/settings/members")).toBe("/orgs/acme/settings/members");
    expect(activeSectionHref("/orgs/acme/settings/members/")).toBe("/orgs/acme/settings/members");
    expect(activeSectionHref("/settings/security")).toBe("/settings/security");
    // The former Server administration page is no section.
    expect(activeSectionHref("/settings/server/connect-forge")).toBeNull();
  });

  it("marks the parent section on a page under it", () => {
    expect(activeSectionHref("/orgs/acme/settings/environments/production/manage")).toBe(
      "/orgs/acme/settings/environments",
    );
    expect(activeSectionHref("/orgs/acme/settings/environments/production/edit")).toBe(
      "/orgs/acme/settings/environments",
    );
    expect(activeSectionHref("/orgs/acme/settings/members/okta%3As.patel%40acme.example/manage")).toBe(
      "/orgs/acme/settings/members",
    );
    expect(activeSectionHref("/orgs/acme/settings/libraries/review")).toBe(
      "/orgs/acme/settings/libraries",
    );
  });

  it("returns exactly one sidebar href for every section", () => {
    const items = (["account", "org"] as const).flatMap((k) => navGroup(globalWithOrg, k, "acme").items);
    expect(items.length).toBe(ACCOUNT_SECTIONS.length + ORG_SECTIONS.length);
    for (const item of items) {
      expect(activeSectionHref(item.href)).toBe(item.href);
      expect(activeSectionHref(`${item.href}/anything/below`)).toBe(item.href);
    }
  });

  it("matches the sidebar's href for an org that needs escaping, however the path spells it", () => {
    expect(activeSectionHref(`${orgHref("a b/c", "environments")}/production/edit`)).toBe(
      orgHref("a b/c", "environments"),
    );
    // A browser leaves `@` unescaped in a path; orgHref escapes it.
    expect(activeSectionHref("/orgs/team@x/settings/members")).toBe(orgHref("team@x", "members"));
  });

  it("marks nothing outside settings or for an unknown section", () => {
    expect(activeSectionHref("/orgs/acme/settings")).toBeNull();
    expect(activeSectionHref("/orgs/acme/settings/nope/x")).toBeNull();
    // Libraries is an org section, not an account one.
    expect(activeSectionHref("/settings/libraries/review")).toBeNull();
    expect(activeSectionHref("/acme/orders-api/runs/1")).toBeNull();
    expect(activeSectionHref("/orgs/acme/members")).toBeNull();
    expect(activeSectionHref("/")).toBeNull();
  });
});

describe("where /settings lands", () => {
  it("sends an admin to their first org's first section", () => {
    expect(settingsHome(admin)).toBe("/orgs/acme/settings/connections");
  });
  it("sends everyone else to their profile", () => {
    expect(settingsHome(member)).toBe("/settings/profile");
    // A global admin before any org exists has no org page to go to.
    expect(settingsHome({ can_administer: true, admin_orgs: [] })).toBe("/settings/profile");
  });
  it("sends a global admin with no org to New organization", () => {
    expect(settingsHome(freshGlobal)).toBe("/orgs/_new");
    expect(legacyRedirect("/settings", "", freshGlobal)).toBe("/orgs/_new");
  });
  it("still sends a global admin with an org to that org", () => {
    expect(settingsHome(globalWithOrg)).toBe("/orgs/acme/settings/connections");
  });
});

describe("a group's URL with no section", () => {
  it("opens the group's first section", () => {
    expect(groupHome("org", "acme")).toBe("/orgs/acme/settings/connections");
    expect(groupHome("account")).toBe("/settings/profile");
  });

  it("escapes the org", () => {
    expect(groupHome("org", "a b")).toBe("/orgs/a%20b/settings/connections");
  });
});

describe("the library review page", () => {
  it("lives under the org's Libraries section", () => {
    expect(libraryReviewHref("acme")).toBe("/orgs/acme/settings/libraries/review");
    expect(libraryReviewHref("a b")).toBe("/orgs/a%20b/settings/libraries/review");
    expect(activeSectionHref(libraryReviewHref("acme"))).toBe(orgHref("acme", "libraries"));
  });
});

describe("section copy", () => {
  it("uses none of the words docs/WRITING.md bans", () => {
    const banned = /\b(posture|substrate|wedge|honest|honestly|load-bearing|first-class|seam|lens|leverage|delve)\b/i;
    for (const s of [...ACCOUNT_SECTIONS, ...ORG_SECTIONS]) {
      expect(`${s.label} ${s.sub}`, s.id).not.toMatch(banned);
    }
  });
});

describe("old URLs", () => {
  it("redirects /account and /account/security, keeping the query", () => {
    expect(legacyRedirect("/account", "?identity_linked=github", member)).toBe(
      "/settings/profile?identity_linked=github",
    );
    expect(legacyRedirect("/account/security", "?enroll=1", member)).toBe(
      "/settings/security?enroll=1",
    );
  });

  it("redirects the library review page into the org", () => {
    expect(legacyRedirect("/settings/libraries/review", "?source=x", admin)).toBe(
      "/orgs/acme/settings/libraries/review?source=x",
    );
  });

  it("redirects /settings to the caller's home", () => {
    expect(legacyRedirect("/settings", "", admin)).toBe("/orgs/acme/settings/connections");
    expect(legacyRedirect("/settings", "", member)).toBe("/settings/profile");
  });

  it("sends the former Server administration page to New organization", () => {
    for (const path of ["/settings/server", "/settings/server/", "/settings/server/connect-forge"]) {
      expect(legacyRedirect(path, "", freshGlobal), path).toBe("/orgs/_new");
      expect(legacyRedirect(path, "", globalWithOrg), path).toBe("/orgs/_new");
      // Anyone else goes where /settings goes.
      expect(legacyRedirect(path, "", admin), path).toBe("/orgs/acme/settings/connections");
      expect(legacyRedirect(path, "", member), path).toBe("/settings/profile");
    }
  });

  it("redirects an org's former Event log to its Security section, keeping the filters", () => {
    const q = "?kind=step_up.refused&from=2026-11-01T09:30Z&cursor=c2VxOjQy";
    expect(legacyRedirect("/orgs/acme/settings/events", q, admin)).toBe(`/orgs/acme/settings/security${q}`);
    expect(legacyRedirect("/orgs/acme/settings/events/", "", admin)).toBe("/orgs/acme/settings/security");
    // The org is decoded and escaped again, as every org href is.
    expect(legacyRedirect("/orgs/a%20b/settings/events", "", admin)).toBe("/orgs/a%20b/settings/security");
    // Whoever follows it: the Security section decides what they may see.
    expect(legacyRedirect("/orgs/globex/settings/events", "", member)).toBe("/orgs/globex/settings/security");
    // Only that path: a section, or a page under one, is not an old URL.
    expect(legacyRedirect("/orgs/acme/settings/security", "", admin)).toBeNull();
    expect(legacyRedirect("/orgs/acme/settings/events/x", "", admin)).toBeNull();
  });

  it("leaves every other path alone", () => {
    expect(legacyRedirect("/settings/profile", "", member)).toBeNull();
    expect(legacyRedirect("/acme/orders-api", "", member)).toBeNull();
  });
});

describe("command palette", () => {
  it("lists every section the caller can reach, and no other", () => {
    const forMember = paletteEntries(member).map((e) => e.href);
    expect(forMember).toEqual(ACCOUNT_SECTIONS.map((s) => accountHref(s.id)));

    // The same rule as the sidebar: no Administer, no org entries.
    expect(paletteEntries({ can_administer: false, admin_orgs: ["acme"] }).map((e) => e.href)).toEqual(
      forMember,
    );

    const forAdmin = paletteEntries(admin).map((e) => e.href);
    expect(forAdmin).toEqual([
      ...ACCOUNT_SECTIONS.map((s) => accountHref(s.id)),
      ...ORG_SECTIONS.map((s) => orgHref("acme", s.id)),
    ]);
  });

  it("names the org on org entries so two orgs' sections are told apart", () => {
    const labels = paletteEntries(twoOrgs).map((e) => e.label);
    expect(labels).toContain("Members · acme");
    expect(labels).toContain("Members · globex");
  });
});
