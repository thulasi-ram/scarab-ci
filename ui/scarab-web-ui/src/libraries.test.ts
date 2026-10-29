import { describe, it, expect } from "vitest";
import {
  defaultsLine,
  diffLines,
  missingLabel,
  segmentError,
  shortHash,
  sourceError,
  toFetchBody,
  updateLine,
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
    expect(updateLine(null)).toBeNull();
    expect(updateLine({ release: "v0.4.0", security_fix: false })).toBe("v0.4.0 is available");
    expect(updateLine({ release: "v0.4.0", security_fix: true })).toMatch(/security/);
    expect(yankReasonError(" ")).toMatch(/reason/);
    expect(yankReasonError("CVE-2026-1")).toBeNull();
    expect(defaultsLine({ state: "installed", missing: [] })).toBeNull();
    expect(defaultsLine({ state: "missing", missing: [{ publisher: "a", name: "b", error: "x" }] })).toMatch(
      /not installed/,
    );
    expect(missingLabel({ publisher: "thulasi-ram", name: "clone" })).toBe("lib:thulasi-ram/clone");
    expect(missingLabel({ publisher: "", name: "" })).toBe("release manifest");
  });
});
