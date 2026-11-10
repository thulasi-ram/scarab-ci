import { describe, expect, it } from "vitest";
import { clampPage, orgPageHref, orgRunStats, pageCount, pageSpan, paginate } from "./org-page";

const HOUR = 60 * 60 * 1000;
const NOW = 1_000 * HOUR;

describe("org page address", () => {
  it("sits under /orgs/ and encodes the org", () => {
    expect(orgPageHref("acme")).toBe("/orgs/acme");
    expect(orgPageHref("a b")).toBe("/orgs/a%20b");
  });
  it("narrows to one connection with ?connection=", () => {
    expect(orgPageHref("acme", "gh acme")).toBe("/orgs/acme?connection=gh%20acme");
  });
});

describe("org run stats", () => {
  const run = (org: string, hoursAgo: number, status: string, gated = false) => ({
    org,
    status,
    created_at: NOW - hoursAgo * HOUR,
    waiting_gates: gated ? [{ step: "approve", kind: "manual", since: 0 }] : [],
  });

  it("counts only the org's runs, by window", () => {
    const runs = [
      run("acme", 1, "succeeded"),
      run("acme", 2, "failed"),
      run("acme", 30, "failed"),
      run("acme", 200, "succeeded"),
      run("globex", 1, "failed"),
    ];
    expect(orgRunStats(runs, "acme", NOW, 50)).toEqual({
      lastDay: 2,
      lastWeek: 3,
      failedLastDay: 1,
      waiting: 0,
      partial: false,
    });
  });

  it("counts runs waiting on a gate whatever their age, suspended ones too", () => {
    const runs = [
      run("acme", 1, "running", true),
      run("acme", 300, "suspended"),
      run("globex", 1, "suspended"),
    ];
    expect(orgRunStats(runs, "acme", NOW, 50).waiting).toBe(2);
  });

  it("is partial when a full list does not reach back a week", () => {
    const runs = [run("acme", 1, "succeeded"), run("globex", 3, "succeeded")];
    expect(orgRunStats(runs, "acme", NOW, 2).partial).toBe(true);
    expect(orgRunStats(runs, "acme", NOW, 3).partial).toBe(false);
    const old = [run("acme", 1, "succeeded"), run("globex", 200, "succeeded")];
    expect(orgRunStats(old, "acme", NOW, 2).partial).toBe(false);
  });
});

describe("paging", () => {
  it("has at least one page", () => {
    expect(pageCount(0, 25)).toBe(1);
    expect(pageCount(25, 25)).toBe(1);
    expect(pageCount(26, 25)).toBe(2);
  });

  it("keeps a page inside the list", () => {
    expect(clampPage(0, 30, 10)).toBe(1);
    expect(clampPage(4, 30, 10)).toBe(3);
    expect(clampPage(2, 30, 10)).toBe(2);
  });

  it("names the positions a page shows", () => {
    expect(pageSpan(1, 10, 32)).toEqual({ from: 1, to: 10 });
    expect(pageSpan(4, 10, 32)).toEqual({ from: 31, to: 32 });
    expect(pageSpan(9, 10, 32)).toEqual({ from: 31, to: 32 });
    expect(pageSpan(1, 10, 0)).toEqual({ from: 0, to: 0 });
  });

  it("slices one page, and nothing past the last", () => {
    const items = Array.from({ length: 7 }, (_, i) => i);
    expect(paginate(items, 1, 3)).toEqual([0, 1, 2]);
    expect(paginate(items, 3, 3)).toEqual([6]);
    expect(paginate(items, 4, 3)).toEqual([]);
  });
});
