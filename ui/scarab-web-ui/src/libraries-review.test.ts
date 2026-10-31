import { describe, it, expect } from "vitest";
import {
  declaredInputs,
  diffLines,
  diffSides,
  foldDiff,
  impactRunLabel,
  inputsChange,
  numberDiff,
  reviewChecks,
  reviewHref,
  sideBySide,
  trustKey,
  trustNote,
  yankButtonLabel,
  yankImpactLine,
} from "./libraries";

const C = "0123456789abcdef0123456789abcdef01234567";

describe("trust", () => {
  it("keys a source without its commit and names why a version is trusted", () => {
    expect(trustKey(`github.com/acme/libs//x.yaml@${C}`)).toBe("github.com/acme/libs//x.yaml");
    const lib = { publisher: "acme", name: "x", source: `github.com/acme/libs//x.yaml@${C}`, first_party: false };
    expect(trustNote(lib, [])).toBeNull();
    expect(trustNote(lib, [{ source: "github.com/acme/libs//x.yaml", trusted_by: "a.kim" }])).toMatch(/by a\.kim/);
    expect(trustNote({ ...lib, publisher: "thulasi-ram", name: "clone", first_party: true }, [])).toMatch(/by default/);
  });
});

describe("yank dialog", () => {
  it("counts Runs and running Steps, and the button says when it cancels", () => {
    expect(yankImpactLine({ runs: [], running_steps: 0 })).toBe("No Run in progress uses this version.");
    expect(yankImpactLine({ runs: [1], running_steps: 0 })).toBe("1 Run in progress uses this version");
    expect(yankImpactLine({ runs: [1, 2, 3], running_steps: 1 })).toBe(
      "3 Runs in progress use this version · 1 Step running now",
    );
    expect(yankButtonLabel(false, 3)).toBe("Yank version");
    expect(yankButtonLabel(true, 0)).toBe("Yank version");
    expect(yankButtonLabel(true, 3)).toBe("Yank and cancel 3 Runs");
    expect(yankButtonLabel(true, 1)).toBe("Yank and cancel 1 Run");
    expect(impactRunLabel({ project: "web", run_number: 418, id: "x" })).toBe("web #418");
    expect(impactRunLabel({ project: "web", id: "0190f8a2deadbeef" })).toBe("web 0190f8a2");
  });
});

const OLD = `kind: library
interface:
  inputs:
    - { name: release, type: string }
    - { name: chart, type: string, required: false, default: ./chart }
steps:
  - id: helm
    image: alpine/helm:3.15.4@sha256:aaaa
    command: [helm, upgrade, x]
  - id: a
  - id: b
  - id: c
  - id: d
  - id: e`;
const NEW = OLD.replace("3.15.4@sha256:aaaa", "3.16.2@sha256:bbbb").replace(
  "    - { name: chart, type: string, required: false, default: ./chart }",
  "    - { name: chart, type: string, required: false, default: ./chart }\n    - name: timeout\n      type: number",
);

/** The server's diff format for OLD → NEW. */
const SERVER_DIFF = [
  " kind: library",
  " interface:",
  "   inputs:",
  "     - { name: release, type: string }",
  "     - { name: chart, type: string, required: false, default: ./chart }",
  "+    - name: timeout",
  "+      type: number",
  " steps:",
  "   - id: helm",
  "-    image: alpine/helm:3.15.4@sha256:aaaa",
  "+    image: alpine/helm:3.16.2@sha256:bbbb",
  "     command: [helm, upgrade, x]",
  "   - id: a",
  "   - id: b",
  "   - id: c",
  "   - id: d",
  "   - id: e",
  "",
].join("\n");

describe("side-by-side diff", () => {
  const lines = diffLines(SERVER_DIFF);
  it("rebuilds both files from the diff", () => {
    expect(diffSides(lines)).toEqual({ old: OLD, new: NEW });
  });
  it("numbers each side, folds far unchanged lines, and pairs a removal with its addition", () => {
    const rows = numberDiff(lines);
    expect(rows[5]).toEqual({ sign: "+", text: "    - name: timeout", new: 6 });
    expect(rows[9]).toEqual({ sign: "-", text: "    image: alpine/helm:3.15.4@sha256:aaaa", old: 8 });
    const folded = foldDiff(rows, 2);
    expect(folded[0]).toEqual({ fold: 3 });
    expect(folded[folded.length - 1]).toEqual({ fold: 4 });
    const { left, right } = sideBySide(folded);
    expect(left.length).toBe(right.length);
    // The two added inputs face blanks; the image lines face each other.
    const at = left.findIndex((c) => "sign" in c && c.sign === "-");
    expect((right[at] as { text: string }).text).toContain("3.16.2");
    expect(left.filter((c) => "blank" in c).length).toBe(2);
  });
});

describe("review checks", () => {
  it("reads the inputs and what the version invokes and runs", () => {
    expect(declaredInputs(NEW)).toEqual(["release", "chart", "timeout"]);
    expect(inputsChange(OLD, NEW)).toBe("adds timeout; no input removed");
    expect(inputsChange(NEW, OLD)).toBe("removes timeout");
    expect(inputsChange(OLD, OLD)).toBe("no input added or removed");
    const content = `${NEW}\n  - id: smoke\n    invoke: lib:acme/http-probe\n  - id: x\n    image: busybox\n    forge_token: true`;
    const checks = reviewChecks(content, false, [{ publisher: "acme", name: "http-probe", content_hash: "2c8f61d0a9e4ffff" }]);
    expect(checks.map((c) => c.tone)).toEqual(["ok", "ok", "warn", "warn", "info"]);
    expect(checks[1]!.text).toBe("Invokes lib:acme/http-probe, installed here at 2c8f61d0a9e4");
    expect(checks[2]!.text).toBe("Not pinned by digest: busybox");
    const clean = reviewChecks(OLD, true, []);
    expect(clean.map((c) => c.tone)).toEqual(["ok", "ok", "info", "ok"]);
  });
  it("links the review page with what is known", () => {
    expect(reviewHref({})).toBe("/settings/libraries/review");
    expect(reviewHref({ source: "s@c", publisher: "acme", name: "", connection: undefined })).toBe(
      "/settings/libraries/review?source=s%40c&publisher=acme",
    );
  });
});
