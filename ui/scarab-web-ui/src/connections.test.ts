import { describe, it, expect } from "vitest";
import {
  assignDefault,
  connectionFor,
  forgeName,
  installationId,
  newOrgNameProblem,
  newOrgPrefill,
  orgNameProblem,
  reposForOrg,
  resyncNote,
  soleOwner,
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

describe("repositories offered to bind", () => {
  const repos = [
    { owner: "acme", name: "api" },
    { owner: "verify-org", name: "team-repo" },
    { owner: "acme", name: "web" },
  ];

  it("offers only the org's own repositories and counts the rest", () => {
    const { offered, hidden } = reposForOrg(repos, "acme");
    expect(offered.map((r) => r.name)).toEqual(["api", "web"]);
    expect(hidden).toBe(1);
  });

  it("matches the owner exactly, not by prefix or case", () => {
    expect(reposForOrg(repos, "Acme").offered).toEqual([]);
    expect(reposForOrg(repos, "acm").offered).toEqual([]);
  });

  it("offers everything when the connection has no org", () => {
    expect(reposForOrg(repos, null)).toEqual({ offered: repos, hidden: 0 });
  });
});

describe("re-sync note", () => {
  it("names what was bound", () => {
    expect(resyncNote({ bound: ["acme/api"], confirmed: 2, skipped: [] })).toBe(
      "Bound 1 new repository: acme/api.",
    );
  });

  it("says when nothing changed", () => {
    expect(resyncNote({ bound: [], confirmed: 3, skipped: [] })).toBe(
      "Already in sync: 3 repositories confirmed.",
    );
  });

  it("lists the repositories the server skipped", () => {
    const note = resyncNote({ bound: [], confirmed: 1, skipped: ["globex/api", "other/x"] });
    expect(note).toContain("Not bound: globex/api, other/x");
    expect(note.startsWith("Already in sync: 1 repository confirmed.")).toBe(true);
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

describe("the org an unassigned connection can take", () => {
  it("is the one owner of its repositories", () => {
    expect(soleOwner([{ owner: "acme" }, { owner: "acme" }])).toBe("acme");
  });

  it("is none for repositories of several owners, or none bound", () => {
    expect(soleOwner([{ owner: "acme" }, { owner: "globex" }])).toBeNull();
    expect(soleOwner([])).toBeNull();
  });
});

describe("the connection that serves a repository", () => {
  it("finds it by bound project and names its forge", () => {
    const conns = [
      { id: "gh", kind: "github", projects: [{ org: "acme", project: "web" }] },
      { id: "fj", kind: "forgejo", projects: [{ org: "acme", project: "infra" }] },
    ];
    expect(connectionFor(conns, "acme", "infra")?.id).toBe("fj");
    expect(connectionFor(conns, "globex", "web")).toBeNull();
    expect(forgeName(connectionFor(conns, "acme", "web")?.kind)).toBe("GitHub");
    expect(forgeName(undefined)).toBe("the forge");
  });
});

describe("connections waiting for an organization", () => {
  const install = {
    id: "github-install-90211",
    kind: "github",
    web_url: "https://github.com",
    projects: [],
  };
  const legacy = {
    id: "forgejo-legacy",
    kind: "forgejo",
    web_url: "https://git.umbrella.dev",
    projects: [
      { owner: "umbrella", name: "api" },
      { owner: "umbrella", name: "web" },
    ],
  };

  it("reads the installation id from an installation's connection id only", () => {
    expect(installationId(install)).toBe("90211");
    expect(installationId({ id: "github-install-90211", kind: "forgejo" })).toBeNull();
    expect(installationId({ id: "gh-acme", kind: "github" })).toBeNull();
    expect(installationId({ id: "github-install-12x", kind: "github" })).toBeNull();
  });

  it("lists the forge, the installation and what is bound", () => {
    expect(waitingFacts(install)).toEqual([
      { label: "forge", value: "GitHub · github.com" },
      { label: "installation", value: "90211" },
      { label: "repositories", value: "none bound" },
    ]);
  });

  it("names the account only through the repositories bound through it", () => {
    expect(waitingFacts(legacy)).toEqual([
      { label: "forge", value: "Forgejo · git.umbrella.dev" },
      { label: "account", value: "umbrella" },
      { label: "repositories", value: "2 bound" },
    ]);
    const mixed = { ...legacy, projects: [...legacy.projects, { owner: "globex", name: "ci" }] };
    expect(waitingFacts(mixed).map((f) => f.label)).not.toContain("account");
  });

  it("starts a new org's name with the account of its bound repositories", () => {
    expect(newOrgPrefill(legacy)).toBe("umbrella");
    expect(newOrgPrefill(install)).toBe("");
  });

  it("refuses an existing org's name for a new org, and every name orgNameProblem refuses", () => {
    expect(newOrgNameProblem("acme", ["acme", "globex"])).toBe(
      "acme is already an organization. Use Assign to an organization.",
    );
    expect(newOrgNameProblem(" umbrella ", ["acme"])).toBeNull();
    expect(newOrgNameProblem("_new", ["acme"])).toBe(orgNameProblem("_new"));
    expect(newOrgNameProblem("", [])).toBe("Name the organization.");
  });

  it("starts an assignment with the owner of its repositories when that is an org", () => {
    expect(assignDefault(legacy, ["acme", "umbrella"])).toBe("umbrella");
    expect(assignDefault(legacy, ["acme", "globex"])).toBe("acme");
    expect(assignDefault(install, [])).toBe("");
  });

  it("says why they wait, for one or several", () => {
    expect(waitingIntro(1)).toBe(
      "Scarab could not prove this account owns an organization of the same name, so it serves none until you choose.",
    );
    expect(waitingIntro(2)).toMatch(/^Scarab could not prove these accounts own organizations/);
  });
});
