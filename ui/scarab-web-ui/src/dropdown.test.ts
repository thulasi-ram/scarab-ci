import { describe, expect, it } from "vitest";
import {
  filterRows,
  firstEnabled,
  lastEnabled,
  openIndex,
  showsSearch,
  stepIndex,
  typeAheadIndex,
} from "./dropdown";

const rows = (...labels: string[]) =>
  labels.map((l) => (l.startsWith("~") ? { label: l.slice(1), disabled: true } : { label: l }));

describe("the search box", () => {
  it("shows above eight options unless the caller says", () => {
    expect(showsSearch(8, undefined)).toBe(false);
    expect(showsSearch(9, undefined)).toBe(true);
    expect(showsSearch(2, true)).toBe(true);
    expect(showsSearch(40, false)).toBe(false);
  });

  it("matches anywhere in the label, ignoring case and outer spaces", () => {
    const r = rows("acme", "Globex", "initech");
    expect(filterRows(r, " GLO ").map((x) => x.label)).toEqual(["Globex"]);
    expect(filterRows(r, "e").map((x) => x.label)).toEqual(["acme", "Globex", "initech"]);
    expect(filterRows(r, "")).toBe(r);
  });
});

describe("arrow keys", () => {
  const r = rows("~acme", "globex", "~hooli", "initech", "~zeta");

  it("skip rows that cannot be picked and stop at the ends", () => {
    expect(stepIndex(r, -1, 1)).toBe(1);
    expect(stepIndex(r, 1, 1)).toBe(3);
    expect(stepIndex(r, 3, 1)).toBe(3);
    expect(stepIndex(r, 3, -1)).toBe(1);
    expect(stepIndex(r, 1, -1)).toBe(1);
  });

  it("Home and End land on the first and last rows that can be picked", () => {
    expect(firstEnabled(r)).toBe(1);
    expect(lastEnabled(r)).toBe(3);
    expect(firstEnabled(rows("~a"))).toBe(-1);
    expect(lastEnabled([])).toBe(-1);
  });

  it("a list opens on the selected row, or the first that can be picked", () => {
    expect(openIndex(r, 3)).toBe(3);
    expect(openIndex(r, 0)).toBe(1); // the selected org is one you do not administer
    expect(openIndex(r, -1)).toBe(1);
  });
});

describe("type-ahead", () => {
  const r = rows("beta", "~bravo", "bet", "alpha", "best");

  it("one letter cycles through the rows it starts, after the current one", () => {
    expect(typeAheadIndex(r, "b", -1)).toBe(0);
    expect(typeAheadIndex(r, "b", 0)).toBe(2);
    expect(typeAheadIndex(r, "bb", 2)).toBe(4);
    expect(typeAheadIndex(r, "bbb", 4)).toBe(0);
  });

  it("a longer prefix stays on the current row while it still matches", () => {
    expect(typeAheadIndex(r, "be", 0)).toBe(0);
    expect(typeAheadIndex(r, "bes", 0)).toBe(4);
    expect(typeAheadIndex(r, "Al", 3)).toBe(3);
  });

  it("finds nothing for no match, a disabled match, or an empty buffer", () => {
    expect(typeAheadIndex(r, "z", 0)).toBe(-1);
    expect(typeAheadIndex(r, "bra", 0)).toBe(-1);
    expect(typeAheadIndex(r, "", 0)).toBe(-1);
    expect(typeAheadIndex([], "a", 0)).toBe(-1);
  });
});
