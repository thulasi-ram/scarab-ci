import { describe, it, expect } from "vitest";
import {
  ACCOUNT_SECTIONS,
  ORG_SECTIONS,
  SERVER_SECTIONS,
  accountHref,
  activeSectionHref,
  groupHome,
  legacyRedirect,
  libraryReviewHref,
  navGroups,
  decodeParam,
  orgHref,
  paletteEntries,
  resolveSection,
  serverHref,
  settingsHome,
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
    expect(resolveSection("org", "profile")).toBeUndefined();
    expect(resolveSection("account", "members")).toBeUndefined();
  });

  it("gives the org's event log its own section, last in the org group", () => {
    expect(resolveSection("org", "events")?.label).toBe("Event log");
    expect(ORG_SECTIONS.map((s) => s.id).slice(-2)).toEqual(["security", "events"]);
    expect(navGroups(admin, "acme")[1]!.items.at(-1)!.href).toBe("/orgs/acme/settings/events");
    // Only the org group has it: your own events stay on Your account → Security.
    expect(resolveSection("account", "events")).toBeUndefined();
  });
});

describe("admin gating", () => {
  it("shows a non-admin only their own group", () => {
    const groups = navGroups(member, undefined);
    expect(groups.map((g) => g.kind)).toEqual(["account"]);
    expect(groups[0]!.items.map((i) => i.href)).toEqual([
      "/settings/profile",
      "/settings/security",
      "/settings/appearance",
    ]);
  });

  it("shows an admin their account and the org in view", () => {
    const groups = navGroups(admin, "acme");
    expect(groups.map((g) => g.kind)).toEqual(["account", "org"]);
    const org = groups[1]!;
    expect(org.heading).toBe("Organization settings");
    expect(org.items.map((i) => i.href)).toContain("/orgs/acme/settings/connections");
  });

  it("shows no org group for an org the caller does not administer", () => {
    expect(navGroups(admin, "globex").map((g) => g.kind)).toEqual(["account"]);
    expect(navGroups(member, "acme").map((g) => g.kind)).toEqual(["account"]);
  });

  it("defaults the org group to the first administrable org outside an org route", () => {
    const groups = navGroups(twoOrgs, undefined);
    expect(groups[1]?.org).toBe("acme");
  });

  it("lists the other administrable orgs to switch to", () => {
    expect(navGroups(twoOrgs, "globex")[1]?.otherOrgs).toEqual(["acme"]);
    expect(navGroups(admin, "acme")[1]?.otherOrgs).toEqual([]);
  });
});

describe("which section the sidebar marks", () => {
  it("marks a section on its own URL, with or without a trailing slash", () => {
    expect(activeSectionHref("/orgs/acme/settings/members")).toBe("/orgs/acme/settings/members");
    expect(activeSectionHref("/orgs/acme/settings/members/")).toBe("/orgs/acme/settings/members");
    expect(activeSectionHref("/settings/security")).toBe("/settings/security");
    expect(activeSectionHref("/settings/server/connect-forge")).toBe("/settings/server/connect-forge");
    expect(activeSectionHref("/settings/server/nope")).toBeNull();
  });

  it("marks the parent section on a page under it", () => {
    expect(activeSectionHref("/orgs/acme/settings/environments/production")).toBe(
      "/orgs/acme/settings/environments",
    );
    expect(activeSectionHref("/orgs/acme/settings/environments/production/edit")).toBe(
      "/orgs/acme/settings/environments",
    );
    expect(activeSectionHref("/orgs/acme/settings/libraries/review")).toBe(
      "/orgs/acme/settings/libraries",
    );
  });

  it("returns exactly one sidebar href for every section", () => {
    for (const item of navGroups(admin, "acme").flatMap((g) => g.items)) {
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

describe("connect a forge (a new org's first connection)", () => {
  it("lives at its own URL, apart from Your account", () => {
    expect(serverHref("connect-forge")).toBe("/settings/server/connect-forge");
    expect(resolveSection("server", "connect-forge")?.label).toBe("Connect a forge");
    expect(resolveSection("account", "connect-forge")).toBeUndefined();
    expect(resolveSection("org", "connect-forge")).toBeUndefined();
  });

  it("is shown to a global admin with no org yet", () => {
    const groups = navGroups(freshGlobal, undefined);
    expect(groups.map((g) => g.kind)).toEqual(["account", "server"]);
    expect(groups[1]!.items.map((i) => i.href)).toEqual(["/settings/server/connect-forge"]);
  });

  it("is shown to a global admin next to the org in view", () => {
    expect(navGroups(globalWithOrg, "acme").map((g) => g.kind)).toEqual(["account", "org", "server"]);
  });

  it("is not shown to an org admin who is not a global admin", () => {
    expect(navGroups(admin, "acme").map((g) => g.kind)).toEqual(["account", "org"]);
    expect(navGroups({ ...admin, global_admin: false }, "acme").map((g) => g.kind)).toEqual([
      "account",
      "org",
    ]);
  });

  it("is in the palette for a global admin only", () => {
    const href = serverHref(SERVER_SECTIONS[0]!.id);
    expect(paletteEntries(freshGlobal).map((e) => e.href)).toContain(href);
    expect(paletteEntries(admin).map((e) => e.href)).not.toContain(href);
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
  it("sends a global admin with no org to Connect a forge", () => {
    expect(settingsHome(freshGlobal)).toBe("/settings/server/connect-forge");
    expect(legacyRedirect("/settings", "", freshGlobal)).toBe("/settings/server/connect-forge");
  });
  it("still sends a global admin with an org to that org", () => {
    expect(settingsHome(globalWithOrg)).toBe("/orgs/acme/settings/connections");
  });
});

describe("a group's URL with no section", () => {
  it("opens the group's first section", () => {
    expect(groupHome("org", "acme")).toBe("/orgs/acme/settings/connections");
    expect(groupHome("server")).toBe("/settings/server/connect-forge");
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
    for (const s of [...ACCOUNT_SECTIONS, ...ORG_SECTIONS, ...SERVER_SECTIONS]) {
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
