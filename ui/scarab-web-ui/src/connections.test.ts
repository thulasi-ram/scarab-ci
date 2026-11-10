import { describe, it, expect } from "vitest";
import {
  forgeName,
  installationId,
  newOrgNameProblem,
  orgNameProblem,
  resyncNote,
  unassignedConnections,
  waitingFacts,
  waitingIntro,
} from "./connections";

describe("unassigned connections", () => {
  it("keeps only the connections with no org", () => {
    const conns = [
      { id: "a", org: "acme" },
      { id: "b", org: null },
      { id: "c" },
      { id: "d", org: "globex" },
    ];
    expect(unassignedConnections(conns).map((c) => c.id)).toEqual(["b", "c"]);
  });
});

describe("re-sync note", () => {
  it("names the repositories that are not enabled, and binds none", () => {
    expect(resyncNote({ available: ["acme/api"], moved: [], confirmed: 2, skipped: [] })).toBe(
      "2 enabled repositories confirmed. 1 repository not enabled: acme/api. Enable them on the organization page.",
    );
  });

  it("says only what was confirmed when nothing else changed", () => {
    expect(resyncNote({ available: [], moved: [], confirmed: 3, skipped: [] })).toBe(
      "3 enabled repositories confirmed.",
    );
  });

  it("names the projects moved to this connection", () => {
    expect(resyncNote({ available: [], moved: ["acme/web"], confirmed: 0 })).toBe(
      "0 enabled repositories confirmed. Moved 1 repository to this connection: acme/web.",
    );
  });

  it("lists the repositories the server skipped", () => {
    const note = resyncNote({ available: [], confirmed: 1, skipped: ["globex/api", "other/x"] });
    expect(note).toContain("Not listed: globex/api, other/x");
    expect(note.startsWith("1 enabled repository confirmed.")).toBe(true);
  });
});

describe("new org name", () => {
  it("accepts a forge account name", () => {
    expect(orgNameProblem("acme")).toBeNull();
    expect(orgNameProblem("  acme-corp ")).toBeNull();
  });

  it("refuses an empty, reserved or multi-segment name", () => {
    expect(orgNameProblem("   ")).not.toBeNull();
    expect(orgNameProblem("_forge")).not.toBeNull();
    expect(orgNameProblem("acme/api")).not.toBeNull();
    expect(orgNameProblem("acme corp")).not.toBeNull();
  });
});

describe("forge names", () => {
  it("names the forge of a connection kind", () => {
    expect(forgeName("github")).toBe("GitHub");
    expect(forgeName("forgejo")).toBe("Forgejo");
    expect(forgeName(undefined)).toBe("the forge");
  });
});

describe("connections waiting for an organization", () => {
  const install = {
    id: "github-install-90211",
    kind: "github",
    web_url: "https://github.com",
    enabled_repos: 0,
  };
  const legacy = {
    id: "forgejo-legacy",
    kind: "forgejo",
    web_url: "https://git.umbrella.dev",
    enabled_repos: 2,
  };

  it("reads the installation id from an installation's connection id only", () => {
    expect(installationId(install)).toBe("90211");
    expect(installationId({ id: "github-install-90211", kind: "forgejo" })).toBeNull();
    expect(installationId({ id: "gh-acme", kind: "github" })).toBeNull();
    expect(installationId({ id: "github-install-12x", kind: "github" })).toBeNull();
  });

  it("lists the forge, the installation and how many repositories are enabled", () => {
    expect(waitingFacts(install)).toEqual([
      { label: "forge", value: "GitHub · github.com" },
      { label: "installation", value: "90211" },
      { label: "repositories", value: "none enabled" },
    ]);
    expect(waitingFacts(legacy)).toEqual([
      { label: "forge", value: "Forgejo · git.umbrella.dev" },
      { label: "repositories", value: "2 enabled" },
    ]);
  });

  it("refuses an existing org's name for a new org, and every name orgNameProblem refuses", () => {
    expect(newOrgNameProblem("acme", ["acme", "globex"])).toBe(
      "acme is already an organization. Use Assign to an organization.",
    );
    expect(newOrgNameProblem(" umbrella ", ["acme"])).toBeNull();
    expect(newOrgNameProblem("_new", ["acme"])).toBe(orgNameProblem("_new"));
    expect(newOrgNameProblem("", [])).toBe("Name the organization.");
  });

  it("says why they wait, for one or several", () => {
    expect(waitingIntro(1)).toBe(
      "Scarab could not prove this account owns an organization of the same name, so it serves none until you choose.",
    );
    expect(waitingIntro(2)).toMatch(/^Scarab could not prove these accounts own organizations/);
  });
});
