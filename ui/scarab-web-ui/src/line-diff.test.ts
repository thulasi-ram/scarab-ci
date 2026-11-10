import { describe, expect, it } from "vitest";
import { diffStat, lineDiff } from "./line-diff";

describe("lineDiff", () => {
  it("keeps equal texts as unchanged lines", () => {
    expect(lineDiff("a\nb", "a\nb")).toEqual([
      { kind: "same", text: "a" },
      { kind: "same", text: "b" },
    ]);
  });

  it("lists a replaced line as removed, then added, between unchanged ones", () => {
    const d = lineDiff("name: x\nmode: audit\nrules:", "name: x\nmode: enforce\nrules:");
    expect(d).toEqual([
      { kind: "same", text: "name: x" },
      { kind: "del", text: "mode: audit" },
      { kind: "add", text: "mode: enforce" },
      { kind: "same", text: "rules:" },
    ]);
    expect(diffStat(d)).toEqual({ added: 1, removed: 1 });
  });

  it("treats an empty side as no lines: a first version is all added, a delete all removed", () => {
    expect(lineDiff("", "a\nb").map((l) => l.kind)).toEqual(["add", "add"]);
    expect(lineDiff("a", "").map((l) => l.kind)).toEqual(["del"]);
    expect(lineDiff("", "")).toEqual([]);
  });

  it("finds an inserted rule without marking the lines around it", () => {
    const before = ["rules:", "  - when: a", "    message: x"].join("\n");
    const after = ["rules:", "  - when: a", "    message: x", "  - when: b", "    message: y"].join("\n");
    const d = lineDiff(before, after);
    expect(d.filter((l) => l.kind === "same").length).toBe(3);
    expect(d.filter((l) => l.kind === "add").map((l) => l.text)).toEqual(["  - when: b", "    message: y"]);
  });
});
