import { describe, it, expect } from "vitest";
import {
  connectionChips,
  connectionHost,
  connectionHref,
  enabledReposLabel,
  forgeName,
  orgNameProblem,
  resyncNote,
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

describe("a connection's list row", () => {
  const healthy = {
    kind: "github",
    base_url: "https://api.github.com",
    web_url: "https://github.com",
    credential_present: true,
    last_delivery_at: 1,
    managed_by_config: false,
  };

  it("names the forge's web host, not its API host", () => {
    expect(connectionHost(healthy)).toBe("github.com");
    expect(connectionHost({ web_url: "", base_url: "https://git.acme.dev:3000/" })).toBe("git.acme.dev:3000");
    expect(connectionHost({ web_url: "not a url", base_url: "" })).toBe("not a url");
  });

  it("links to the connection's page under the org's Connections", () => {
    expect(connectionHref("acme", "forgejo-7f3a91c2")).toBe("/orgs/acme/settings/connections/forgejo-7f3a91c2");
    expect(connectionHref("a b", "x/y")).toBe("/orgs/a%20b/settings/connections/x%2Fy");
  });

  it("carries no tag when the credential resolves and a delivery came in", () => {
    expect(connectionChips(healthy)).toEqual([]);
  });

  it("puts a missing credential first, then no deliveries, then configuration", () => {
    const chips = connectionChips({
      ...healthy,
      credential_present: false,
      last_delivery_at: null,
      managed_by_config: true,
    });
    expect(chips.map((c) => [c.label, c.tone])).toEqual([
      ["credential missing", "danger"],
      ["no deliveries yet", "muted"],
      ["managed by configuration", "muted"],
    ]);
    expect(chips[1]!.title).toContain("GitHub");
  });

  it("counts enabled repositories in words", () => {
    expect(enabledReposLabel(1)).toBe("1 enabled repository");
    expect(enabledReposLabel(0)).toBe("0 enabled repositories");
  });
});
