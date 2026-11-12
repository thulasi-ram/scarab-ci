import { describe, it, expect } from "vitest";
import {
  diffLines,
  libraryChips,
  defaultLibraryName,
  isLibraryName,
  libraryId,
  libraryManageHref,
  libraryNameError,
  removedLibraryYanks,
  strayTrust,
  trustedSourceRows,
  versionLabel,
  yanksOfLibrary,
  shortHash,
  refError,
  sourceError,
  splitSource,
  toFetchBody,
  yankReasonError,
} from "./libraries";

const C = "0123456789abcdef0123456789abcdef01234567";

describe("sourceError", () => {
  it("reads the file, with or without a ref after @", () => {
    expect(sourceError("github.com/acme/libs//deploy/helm.yaml")).toBeNull();
    expect(sourceError(`https://github.com/acme/libs//x.yml@${C}`)).toBeNull();
    expect(sourceError("github.com/acme/libs//x.yaml@main")).toBeNull();
    expect(sourceError("")).toBeNull();
    expect(sourceError("github.com/acme/libs/x.yaml")).toMatch(/\/\//);
    expect(sourceError("github.com/acme//x.yaml")).toMatch(/<host>\/<owner>\/<repo>/);
    expect(sourceError("github.com/acme/libs//../x.yaml")).toMatch(/repo-relative/);
    expect(sourceError("github.com/acme/libs//x.sh")).toMatch(/\.yaml/);
  });
});

describe("splitSource and refError", () => {
  it("splits at the first @, since a ref may hold one and a file may not", () => {
    expect(splitSource(` https://github.com/acme/libs//d.yaml@release@2 `)).toEqual({
      source: "github.com/acme/libs//d.yaml",
      ref: "release@2",
    });
    expect(splitSource("github.com/acme/libs//d.yaml")).toEqual({ source: "github.com/acme/libs//d.yaml", ref: "" });
  });
  it("refuses a ref with spaces", () => {
    expect(refError("feature/x")).toBeNull();
    expect(refError("")).toBeNull();
    expect(refError("my branch")).toMatch(/spaces/);
    expect(refError("..")).toMatch(/not a ref/);
    expect(refError("main\u202ex")).toMatch(/printable ASCII/);
  });
});

describe("toFetchBody", () => {
  const blank = { source: "", ref: "", connection: "", name: "" };
  it("pins a full commit in the source and sends a branch or tag as ref", () => {
    expect(toFetchBody({ ...blank, source: " github.com/acme/libs//d.yaml ", ref: C })).toEqual({
      body: { source: `github.com/acme/libs//d.yaml@${C}` },
    });
    expect(toFetchBody({ ...blank, source: "github.com/acme/libs//d.yaml", ref: "v1.2.0" })).toEqual({
      body: { source: "github.com/acme/libs//d.yaml", ref: "v1.2.0" },
    });
    // A review link from before the Ref field: the commit after @.
    expect(toFetchBody({ ...blank, source: `github.com/acme/libs//d.yaml@${C}` })).toEqual({
      body: { source: `github.com/acme/libs//d.yaml@${C}` },
    });
    expect(
      toFetchBody({ source: "github.com/acme/libs//d.yaml", ref: "main", connection: "gh", name: "platform::helm" }),
    ).toEqual({
      body: { source: "github.com/acme/libs//d.yaml", ref: "main", connection: "gh", name: "platform::helm" },
    });
  });
  it("refuses an empty source or ref, or a bad name", () => {
    expect(toFetchBody(blank).error).toMatch(/source is required/);
    expect(toFetchBody({ ...blank, source: "github.com/acme/libs//d.yaml" }).error).toMatch(/branch, tag or commit/);
    for (const name of ["-x", "a b", "x${y}", "a:b"]) {
      expect(toFetchBody({ ...blank, source: "github.com/acme/libs//d.yaml", ref: "main", name }).error).toMatch(
        /is not a library name: a library name is parts of letters/,
      );
    }
    expect(libraryNameError("")).toBeNull();
  });
});

describe("the library name rule", () => {
  it("takes parts joined by :: or /", () => {
    for (const good of [
      "thulasi-ram::scarab-ci::clone",
      "acme::scarab-libs::deploy-helm",
      "deploy",
      "acme::platform/helm",
      "_x.y-z",
      "x".repeat(255),
    ]) {
      expect(isLibraryName(good), good).toBe(true);
    }
  });
  it("refuses an empty part, a lone colon, a leading . or -, and punctuation", () => {
    for (const bad of [
      "",
      "acme:x",
      "::x",
      "x::",
      "x/",
      "/x",
      "a b",
      "-x",
      "a::.b",
      "a${x}",
      "a//b",
      "a::::b",
      ":",
      ":::",
      "a:::b",
      'a"b',
      "a'b",
      "a@b",
      "a#b",
      "a%b",
      "a?b",
      "a\\b",
      "a~b",
      "a,b",
      "a\tb",
      "caf\u00e9",
      "x".repeat(256),
    ]) {
      expect(isLibraryName(bad), JSON.stringify(bad)).toBe(false);
    }
  });
  it("defaults to <owner>::<repo>::<file name>", () => {
    expect(defaultLibraryName("github.com/acme/scarab-libs//deploy-helm.yaml")).toBe("acme::scarab-libs::deploy-helm");
    expect(defaultLibraryName(`https://github.com/acme/libs//ci/build.yml@${C}`)).toBe("acme::libs::build");
    expect(defaultLibraryName("github.com/acme/libs//x.yaml.yml")).toBe("acme::libs::x.yaml");
    expect(defaultLibraryName("github.com/acme/libs")).toBeNull();
    expect(defaultLibraryName("")).toBeNull();
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
  it("names an installation in the URL by its id", () => {
    const l = { id: "b72d4f1e06", name: "acme::scarab-libs::deploy-helm.v2" };
    expect(libraryId(l)).toBe("lib:acme::scarab-libs::deploy-helm.v2");
    expect(libraryManageHref("ac me", l)).toBe("/orgs/ac%20me/settings/libraries/b72d4f1e06/manage");
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
  it("lists every yank of a library on its page, the installed version's marked", () => {
    const y = (content_hash: string, name: string, yanked_at: number) => ({ content_hash, name, yanked_at });
    const helm = { name: "acme::helm", content_hash: "cc" };
    const yanks = [
      y("aa", "acme::helm", 1),
      y("bb", "acme::probe", 2),
      // Installed as acme::helm, first fetched for another name.
      y("cc", "acme::helm-old", 3),
    ];
    expect(yanksOfLibrary(yanks, helm).map((x) => [x.content_hash, x.installed])).toEqual([
      ["cc", true],
      ["aa", false],
    ]);
    expect(yanksOfLibrary(yanks, { ...helm, content_hash: "dd" }).map((x) => x.installed)).toEqual([false]);
  });
  it("keeps on the list only the yanks and trust no installation holds", () => {
    const installed = [
      { name: "acme::x", content_hash: "aa", source: `github.com/acme/libs//x.yaml@${C}` },
    ];
    const yank = (content_hash: string, name: string) => ({ content_hash, name: `acme::${name}`, yanked_at: 0 });
    // An earlier version of an installed library is on its manage page, not here.
    expect(
      removedLibraryYanks([yank("aa", "x"), yank("a0", "x"), yank("bb", "gone")], installed).map((y) => y.content_hash),
    ).toEqual(["bb"]);
    expect(
      strayTrust([{ source: "github.com/acme/libs//x.yaml" }, { source: "github.com/acme/libs//y.yaml" }], installed),
    ).toEqual([{ source: "github.com/acme/libs//y.yaml" }]);
  });
  it("names a version by its source and a short commit", () => {
    expect(versionLabel(`github.com/acme/libs//x.yaml@${C}`)).toBe("github.com/acme/libs//x.yaml@0123456");
    expect(versionLabel("github.com/acme/libs//x.yaml")).toBe("github.com/acme/libs//x.yaml");
  });
  it("matches each trusted source to the installations that come from it", () => {
    const installed = [
      { id: "1", source: `github.com/acme/libs//x.yaml@${C}` },
      { id: "2", source: `github.com/acme/libs//x.yaml@${"f".repeat(40)}` },
      { id: "3", source: `github.com/acme/libs//x.yaml.bak@${C}` },
    ];
    const rows = trustedSourceRows(
      [{ source: "github.com/acme/libs//x.yaml" }, { source: "github.com/acme/libs//y.yaml" }],
      installed,
    );
    expect(rows.map((r) => r.libraries.map((l) => l.id))).toEqual([["1", "2"], []]);
  });
});
