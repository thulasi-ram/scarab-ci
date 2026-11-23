import { describe, expect, it } from "vitest";
import { originPhrase, phraseText, pullNumber, runHeadline, statusGlyph } from "./run-row";

const read = (o: Parameters<typeof originPhrase>[0]) => phraseText(originPhrase(o));
const SHA = "bc99147aa0f1c2d3e4f5a6b7c8d9e0f1a2b3c4d5";

describe("originPhrase", () => {
  it("a push reads branch · hash · actor", () => {
    expect(read({ kind: "push", ref: "refs/heads/main", sha: SHA, actor: "a.kim" })).toBe("main · bc99147 · a.kim");
  });

  it("a pull request recorded as refs/pull/N/head reads #N → base, the hash once", () => {
    const parts = originPhrase({ kind: "pull_request", ref: "refs/pull/212/head", sha: SHA, pr: 212, base: "main", actor: "m.diaz" });
    expect(phraseText(parts)).toBe("#212 → main · bc99147 · m.diaz");
    expect(parts.filter((p) => p.t === "sha")).toHaveLength(1);
  });

  it("a pull request whose ref names its head branch shows it", () => {
    expect(read({ kind: "pull_request", ref: "refs/heads/feat/x", sha: SHA, pr: 212, base: "main" })).toBe(
      "#212 feat/x → main · bc99147",
    );
  });

  it("a refused pull request takes its number from the ref", () => {
    expect(pullNumber("refs/pull/205/head")).toBe(205);
    expect(read({ kind: "pull_request", ref: "refs/pull/205/head", actor: "outside-contrib" })).toBe("#205 · outside-contrib");
  });

  it("tag, manual, api and cron name their kind", () => {
    expect(read({ kind: "tag", ref: "refs/tags/v0.7.2", sha: SHA })).toBe("tag v0.7.2 · bc99147");
    expect(read({ kind: "manual", ref: "refs/heads/feat/x", actor: "j.rao" })).toBe("manual on feat/x · j.rao");
    expect(read({ kind: "api", ref: "refs/heads/main" })).toBe("API on main");
    expect(read({ kind: "cron", ref: "refs/heads/main", title: "0 2 * * *", sha: SHA })).toBe(
      "schedule 0 2 * * * on main · bc99147",
    );
    expect(read({ kind: "cron", title: "0 6 * * 1" })).toBe("schedule 0 6 * * 1");
  });

  it("a Run with no trigger says so", () => {
    expect(read({ sha: SHA })).toBe("no trigger recorded · bc99147");
  });
});

describe("runHeadline", () => {
  it("a cron Run reads Scheduled run, not its expression", () => {
    expect(runHeadline("cron", "0 2 * * *")).toEqual({ text: "Scheduled run", muted: false });
  });
  it("a Run with no headline gets a muted stand-in", () => {
    expect(runHeadline("api", null)).toEqual({ text: "Started from the API", muted: true });
    expect(runHeadline("push", "")).toEqual({ text: "No headline", muted: true });
    expect(runHeadline("push", "Bump tokio")).toEqual({ text: "Bump tokio", muted: false });
  });
});

describe("statusGlyph", () => {
  it("running and pending draw a ring with no glyph", () => {
    expect(statusGlyph("running")).toBeNull();
    expect(statusGlyph("pending")).toBeNull();
    expect(statusGlyph("dead_lettered")).toBe("exclamation");
  });
});
