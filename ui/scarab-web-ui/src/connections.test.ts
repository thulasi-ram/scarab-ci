import { describe, it, expect } from "vitest";
import { orgNameProblem, reposForOrg, resyncNote, soleOwner, unassignedConnections } from "./connections";

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
