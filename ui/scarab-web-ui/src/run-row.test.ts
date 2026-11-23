import { describe, expect, it } from "vitest";
import {
  authorSlot,
  cutName,
  dayGroups,
  dayLabel,
  daySummary,
  phraseText,
  placeReplaced,
  pullNumber,
  runHeadline,
  statusGlyph,
  statusPhrase,
  statusText,
  wherePhrase,
} from "./run-row";

const read = (o: Parameters<typeof wherePhrase>[0]) => phraseText(wherePhrase(o));

describe("wherePhrase", () => {
  it("a push reads its branch", () => {
    expect(read({ kind: "push", ref: "refs/heads/main" })).toBe("main");
  });

  it("a pull request recorded as refs/pull/N/head reads #N → base", () => {
    expect(read({ kind: "pull_request", ref: "refs/pull/212/head", pr: 212, base: "main" })).toBe("#212 → main");
  });

  it("a pull request whose ref names its head branch shows it", () => {
    expect(read({ kind: "pull_request", ref: "refs/heads/feat/x", pr: 212, base: "main" })).toBe("#212 feat/x → main");
  });

  it("a refused pull request takes its number from the ref", () => {
    expect(pullNumber("refs/pull/205/head")).toBe(205);
    expect(read({ kind: "pull_request", ref: "refs/pull/205/head" })).toBe("#205");
  });

  it("tag, manual, api and cron name their kind", () => {
    expect(read({ kind: "tag", ref: "refs/tags/v0.7.2" })).toBe("tag v0.7.2");
    expect(read({ kind: "manual", ref: "refs/heads/feat/x" })).toBe("manual on feat/x");
    expect(read({ kind: "api", ref: "refs/heads/main" })).toBe("API on main");
    expect(read({ kind: "cron", ref: "refs/heads/main", title: "0 2 * * *" })).toBe("schedule 0 2 * * * on main");
  });

  it("a Run with no trigger says so", () => {
    expect(read({})).toBe("no trigger recorded");
  });
});

describe("authorSlot", () => {
  it("a schedule has no author and says so; anything else gets a dash", () => {
    expect(authorSlot("cron", null)).toEqual({ text: "schedule", muted: true });
    expect(authorSlot("api", null)).toEqual({ text: "—", muted: true });
    expect(authorSlot("push", "a.kim")).toEqual({ text: "a.kim", muted: false });
  });
});

describe("cutName", () => {
  it("cuts a long name to 24 characters with the ellipsis inside the count", () => {
    const cut = cutName("integration-tests-against-postgres-and-minio-on-kind");
    expect(cut).toBe("integration-tests-again…");
    expect([...cut]).toHaveLength(24);
    expect(cutName("ci")).toBe("ci");
    expect(cutName("x".repeat(24))).toBe("x".repeat(24));
  });
});

describe("runHeadline", () => {
  it("a cron Run reads Scheduled run, not its expression", () => {
    expect(runHeadline("cron", "0 2 * * *")).toEqual({ text: "Scheduled run", muted: false });
  });
  it("a Run with no headline gets a muted stand-in", () => {
    expect(runHeadline("api", null)).toEqual({ text: "Started from the API", muted: true });
    expect(runHeadline("push", "")).toEqual({ text: "No headline", muted: true });
  });
});

describe("statusGlyph", () => {
  it("running and pending draw a ring with no glyph", () => {
    expect(statusGlyph("running")).toBeNull();
    expect(statusGlyph("pending")).toBeNull();
    expect(statusGlyph("dead_lettered")).toBe("exclamation");
  });
});

describe("statusPhrase", () => {
  const text = (...a: Parameters<typeof statusPhrase>) => statusText(statusPhrase(...a));

  it("a failed Run names its first failed step, and how many more failed", () => {
    const p = statusPhrase({ status: "failed", failed_step: "test[os=macos]", failed_steps_count: 2 }, null);
    expect(p).toEqual({ lead: "Failed at", step: "test · macos +1", tone: "failed" });
    expect(text({ status: "failed", failed_step: "migrate", failed_steps_count: 1 }, null)).toBe("Failed at migrate");
  });

  it("a suspended Run says what it waits for", () => {
    expect(text({ status: "suspended", waiting_for: { step: "approve", kind: "environment_approval" } }, null)).toBe(
      "Waiting for approval",
    );
    expect(text({ status: "suspended", waiting_for: { step: "soak", kind: "timer" } }, null)).toBe("Waiting for a timer");
  });

  it("an older server's row, without the new fields, reads its status word", () => {
    expect(text({ status: "failed" }, null)).toBe("Failed");
    expect(text({ status: "suspended" }, null)).toBe("Suspended");
  });

  it("a replaced Run names its replacer when it is on the page", () => {
    expect(statusPhrase({ status: "cancelled" }, { number: 130 })).toEqual({
      lead: "Replaced by #130",
      step: null,
      tone: "cancelled",
    });
    expect(text({ status: "cancelled" }, { number: null })).toBe("Replaced");
  });
});

// A list row as RepoView builds it: a Run or a refused trigger, newest first.
type Row = { kind: string; at: number; run?: { id: string; run_number: number; status: string; replaced_by?: string } };
const run = (n: number, at: number, replacedBy?: number, status = "cancelled"): Row => ({
  kind: "run",
  at,
  run: { id: `r${n}`, run_number: n, status, replaced_by: replacedBy ? `r${replacedBy}` : undefined },
});
const refused = (at: number): Row => ({ kind: "refused", at });
const shape = (rows: Row[]) =>
  placeReplaced(rows, (r) => r.run ?? null).map((p) => ({
    n: p.row.run?.run_number ?? "refused",
    child: p.child,
    replaced: p.replaced?.number,
  }));

describe("placeReplaced", () => {
  it("a chain of two replaced Runs sits under the newest, each naming its own replacer", () => {
    // #12 replaced #11, which replaced #10; #9 came between them in time.
    const rows = [run(12, 500, undefined, "failed"), run(11, 400, 12), run(9, 300, undefined, "succeeded"), run(10, 200, 11)];
    expect(shape(rows)).toEqual([
      { n: 12, child: false, replaced: undefined },
      { n: 11, child: true, replaced: 12 },
      { n: 10, child: true, replaced: 11 },
      { n: 9, child: false, replaced: undefined },
    ]);
  });

  it("a Run whose replacer is filtered out keeps its place and reads Replaced", () => {
    // #12 is hidden by a filter: #11 stays in time order, and #10 still sits under #11.
    const rows = [run(11, 400, 12), refused(350), run(10, 200, 11)];
    expect(shape(rows)).toEqual([
      { n: 11, child: false, replaced: null },
      { n: 10, child: true, replaced: 11 },
      { n: "refused", child: false, replaced: undefined },
    ]);
  });
});

describe("day groups", () => {
  // Local times, so the test holds in any time zone.
  const at = (d: number, h: number, m = 0) => new Date(2026, 10, d, h, m).getTime();
  const now = at(24, 15);

  it("names today, yesterday, then the weekday and date", () => {
    expect(dayLabel(at(24, 0, 1), now)).toBe("Today");
    expect(dayLabel(at(23, 23, 59), now)).toBe("Yesterday");
    expect(dayLabel(at(22, 9), now)).toBe("Sunday 22 Nov");
    expect(dayLabel(new Date(2025, 10, 22, 9).getTime(), now)).toBe("Saturday 22 Nov 2025");
  });

  it("counts Runs and refused triggers per day, and keeps a replaced Run with its replacer", () => {
    // #5 (yesterday) was replaced by #6 (today): it is listed under #6, in today's group.
    const rows = [run(6, at(24, 9), undefined, "failed"), refused(at(24, 8)), run(5, at(23, 23), 6), run(4, at(23, 10), undefined, "succeeded")];
    const groups = dayGroups(placeReplaced(rows, (r) => r.run ?? null), (r) => r.run?.status ?? "refused", now);
    expect(groups.map((g) => [g.label, g.items.map((i) => i.row.run?.run_number ?? "refused")])).toEqual([
      ["Today", [6, 5, "refused"]],
      ["Yesterday", [4]],
    ]);
    expect(daySummary(groups[0])).toEqual({ runs: "3 runs", failed: "2 failed or refused" });
    expect(daySummary(groups[1])).toEqual({ runs: "1 run", failed: null });
  });
});
