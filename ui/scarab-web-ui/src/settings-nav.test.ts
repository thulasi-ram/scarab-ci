import { describe, it, expect } from "vitest";
import {
  ACCOUNT_SECTIONS,
  ORG_SECTIONS,
  accountHref,
  legacyRedirect,
  navGroups,
  orgHref,
  paletteEntries,
  resolveSection,
  settingsHome,
} from "./settings-nav";

type Who = { can_administer: boolean; admin_orgs: string[] };
const member: Who = { can_administer: false, admin_orgs: [] };
const admin: Who = { can_administer: true, admin_orgs: ["acme"] };
const twoOrgs: Who = { can_administer: true, admin_orgs: ["acme", "globex"] };

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

describe("where /settings lands", () => {
  it("sends an admin to their first org's first section", () => {
    expect(settingsHome(admin)).toBe("/orgs/acme/settings/connections");
  });
  it("sends everyone else to their profile", () => {
    expect(settingsHome(member)).toBe("/settings/profile");
    // A global admin before any org exists has no org page to go to.
    expect(settingsHome({ can_administer: true, admin_orgs: [] })).toBe("/settings/profile");
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
