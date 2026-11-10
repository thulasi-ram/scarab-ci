import { describe, it, expect } from "vitest";
import {
  diffLines,
  libraryChips,
  libraryId,
  libraryKey,
  libraryManageHref,
  parseLibraryKey,
  segmentError,
  strayTrust,
  strayYanks,
  shortHash,
  sourceError,
  toFetchBody,
  yankReasonError,
} from "./libraries";

const C = "0123456789abcdef0123456789abcdef01234567";

describe("sourceError", () => {
  it("accepts a commit-pinned source and refuses one that could move", () => {
    expect(sourceError(`github.com/acme/libs//deploy/helm.yaml@${C}`)).toBeNull();
    expect(sourceError(`https://github.com/acme/libs//x.yml@${C}`)).toBeNull();
    expect(sourceError("")).toBeNull();
    expect(sourceError("github.com/acme/libs//x.yaml@main")).toMatch(/full commit id/);
    expect(sourceError("github.com/acme/libs//x.yaml")).toMatch(/commit/);
    expect(sourceError(`github.com/acme/libs/x.yaml@${C}`)).toMatch(/\/\//);
    expect(sourceError(`github.com/acme//x.yaml@${C}`)).toMatch(/<host>\/<owner>\/<repo>/);
    expect(sourceError(`github.com/acme/libs//../x.yaml@${C}`)).toMatch(/repo-relative/);
    expect(sourceError(`github.com/acme/libs//x.sh@${C}`)).toMatch(/\.yaml/);
  });
});

describe("toFetchBody", () => {
  it("sends only what was filled in", () => {
    expect(
      toFetchBody({ source: ` github.com/acme/libs//d.yaml@${C} `, connection: "", publisher: "", name: "" }),
    ).toEqual({ body: { source: `github.com/acme/libs//d.yaml@${C}` } });
    expect(
      toFetchBody({ source: `github.com/acme/libs//d.yaml@${C}`, connection: "gh", publisher: "platform", name: "helm" }),
    ).toEqual({
      body: { source: `github.com/acme/libs//d.yaml@${C}`, connection: "gh", publisher: "platform", name: "helm" },
    });
  });
  it("refuses an empty source or a bad name", () => {
    expect(toFetchBody({ source: "", connection: "", publisher: "", name: "" }).error).toMatch(/required/);
    expect(
      toFetchBody({ source: `github.com/acme/libs//d.yaml@${C}`, connection: "", publisher: "-x", name: "" }).error,
    ).toMatch(/not a valid name/);
    expect(segmentError("thulasi-ram")).toBeNull();
  });
});

describe("diffLines", () => {
  it("reads the server's sign column", () => {
    expect(diffLines(" a\n-b\n+c\n")).toEqual([
      { sign: " ", text: "a" },
      { sign: "-", text: "b" },
      { sign: "+", text: "c" },
    ]);
    expect(diffLines("")).toEqual([]);
  });
});

describe("lines", () => {
  it("say what an admin needs to know", () => {
    expect(shortHash("a".repeat(64))).toBe("aaaaaaaaaaaa");
    expect(yankReasonError(" ")).toMatch(/reason/);
    expect(yankReasonError("CVE-2026-1")).toBeNull();
  });
});

describe("the list and manage pages", () => {
  it("names an installation in one URL segment that splits back", () => {
    const l = { publisher: "acme", name: "deploy-helm.v2" };
    expect(libraryId(l)).toBe("lib:acme/deploy-helm.v2");
    expect(libraryKey(l)).toBe("acme~deploy-helm.v2");
    expect(parseLibraryKey(libraryKey(l))).toEqual(l);
    expect(libraryManageHref("ac me", l)).toBe("/orgs/ac%20me/settings/libraries/acme~deploy-helm.v2/manage");
  });
  it("refuses a segment that is not a publisher and a name", () => {
    for (const bad of ["acme", "acme~", "~x", "a~b~c", "acme~.x", "review"]) {
      expect(parseLibraryKey(bad)).toBeNull();
    }
  });
  it("tags a yanked version first, then whether its source is trusted", () => {
    const source = `github.com/acme/libs//x.yaml@${C}`;
    const trusted = [{ source: "github.com/acme/libs//x.yaml", trusted_by: "a.kim" }];
    expect(libraryChips({ source }, []).map((c) => c.label)).toEqual(["untrusted source"]);
    expect(libraryChips({ source }, trusted).map((c) => [c.label, c.tone])).toEqual([["trusted source", "ok"]]);
    const yanked = libraryChips({ source, yanked: { reason: "leaks a token", yanked_by: "j.lee" } }, trusted);
    expect(yanked.map((c) => c.label)).toEqual(["yanked", "trusted source"]);
    expect(yanked[0]!.title).toBe("Yanked by j.lee: leaks a token");
  });
  it("keeps on the list only the yanks and trust no installation holds", () => {
    const installed = [{ content_hash: "aa", source: `github.com/acme/libs//x.yaml@${C}` }];
    expect(strayYanks([{ content_hash: "aa" }, { content_hash: "bb" }], installed)).toEqual([{ content_hash: "bb" }]);
    expect(
      strayTrust([{ source: "github.com/acme/libs//x.yaml" }, { source: "github.com/acme/libs//y.yaml" }], installed),
    ).toEqual([{ source: "github.com/acme/libs//y.yaml" }]);
  });
});
