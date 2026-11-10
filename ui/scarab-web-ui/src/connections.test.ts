import { describe, it, expect } from "vitest";
import {
  connectionFor,
  forgeName,
  orgNameProblem,
  reposForOrg,
  resyncNote,
  soleOwner,
  unassignedConnections,
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
