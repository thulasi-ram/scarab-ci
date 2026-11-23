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
  rowStatus,
  arrivedLabel,
  deployTag,
  lineTwoParts,
  kindFirst,
  kindText,
  splitArrived,
} from "./run-row";

const line = (o: Parameters<typeof kindFirst>[0]) => {
  const { kind, where } = kindFirst(o);
  return [kindText(kind), phraseText(where)];
};

describe("line 2, kind first", () => {
  it("a push names its branch", () => {
    expect(line({ kind: "push", ref: "refs/heads/main" })).toEqual(["push", "main"]);
  });

  it("a pull request reads its number and head branch, without the base", () => {
    const o = { kind: "pull_request", ref: "refs/pull/213/head", pr: 213, head: "feat/org-page", base: "main" };
    expect(line(o)).toEqual(["pr #213", "feat/org-page"]);
    expect(kindFirst(o).tight).toBe(false);
    expect(kindFirst({ kind: "pull_request", pr: 212 }).kind.token).toEqual({ t: "pr", n: 212 });
  });

  it("without the head branch, → base follows the trigger with no column gap", () => {
    const o = { kind: "pull_request", ref: "refs/pull/212/head", pr: 212, base: "main" };
    expect(line(o)).toEqual(["pr #212", "→ main"]);
    expect(kindFirst(o).tight).toBe(true);
    // A ref that names a branch stands in for the head.
    expect(line({ kind: "pull_request", ref: "refs/heads/feat/x", pr: 212, base: "main" })).toEqual(["pr #212", "feat/x"]);
    expect(kindFirst({ kind: "push", ref: "refs/heads/main" }).tight).toBe(false);
  });

  it("a refused pull request takes its number from the ref", () => {
    expect(pullNumber("refs/pull/205/head")).toBe(205);
    expect(line({ kind: "pull_request", ref: "refs/pull/205/head" })).toEqual(["pr #205", ""]);
  });

  it("a tag names itself in the first column and nothing after", () => {
    expect(line({ kind: "tag", ref: "refs/tags/v0.7.2" })).toEqual(["tag v0.7.2", ""]);
  });

  it("manual and API say where they ran; a schedule shows its expression", () => {
    expect(line({ kind: "manual", ref: "refs/heads/feat/x" })).toEqual(["manual", "on feat/x"]);
    expect(line({ kind: "api", ref: "refs/heads/main" })).toEqual(["api", "on main"]);
    expect(line({ kind: "cron", ref: "refs/heads/main", title: "0 2 * * *" })).toEqual(["schedule", "0 2 * * *"]);
    expect(line({ kind: "upstream_run", ref: "refs/heads/main" })).toEqual(["upstream run", "on main"]);
  });

  it("every trigger word is lowercase, so they read at one size", () => {
    const kinds = ["push", "pull_request", "tag", "release", "cron", "manual", "api", "upstream_run"];
    const words = kinds.map((kind) => kindFirst({ kind, pr: 7, ref: "refs/tags/v1" }).kind.word);
    expect(words).toEqual(["push", "pr", "tag", "release", "schedule", "manual", "api", "upstream run"]);
  });

  it("a Run with no trigger says so, muted", () => {
    expect(kindFirst({}).kind).toEqual({ word: "no trigger recorded", token: null, muted: true });
  });
});

describe("line 2's parts", () => {
  const parts = (o: Parameters<typeof kindFirst>[0], sha: string | null, actor: string | null, deploy: boolean, env: string | null) =>
    lineTwoParts({ ...kindFirst(o), sha, author: authorSlot(o.kind, actor), deploy: deployTag(deploy, env) });

  it("a deploy Run's Environment comes last, after the author", () => {
    expect(parts({ kind: "push", ref: "refs/heads/main" }, "3f9c2ab9e0", "a.kim", true, "production")).toEqual([
      "push",
      "main",
      "3f9c2ab",
      "a.kim",
      "production",
    ]);
  });

  it("a deploy Run with no Environment reads deploy; other Runs have no tag", () => {
    expect(parts({ kind: "tag", ref: "refs/tags/v0.7.2" }, "3f9c2ab", "a.kim", true, null)).toEqual(["tag v0.7.2", "3f9c2ab", "a.kim", "deploy"]);
    expect(parts({ kind: "cron", ref: "refs/heads/main", title: "0 2 * * *" }, null, null, false, "production")).toEqual([
      "schedule",
      "0 2 * * *",
      "—",
      "—",
    ]);
  });

  it("an Environment is cut at 18 characters; the title names it whole", () => {
    expect(deployTag(true, "preview-org-page-repo-enable")).toEqual({
      text: "preview-org-page-…",
      title: "deploy run into preview-org-page-repo-enable",
    });
    expect(deployTag(true, null)).toEqual({ text: "deploy", title: "deploy run" });
  });
});

describe("Runs that arrive between polls", () => {
  const key = (r: { id: string }) => r.id;

  it("holds back the rows not on screen and keeps the order of both parts", () => {
    const latest = [{ id: "r9" }, { id: "r8" }, { id: "r7" }, { id: "r6" }];
    const { shown, arrived } = splitArrived(latest, key, new Set(["r7", "r6"]));
    expect(shown.map(key)).toEqual(["r7", "r6"]);
    expect(arrived.map(key)).toEqual(["r9", "r8"]);
    expect(arrivedLabel(arrived.length)).toBe("2 new Runs");
    expect(arrivedLabel(1)).toBe("1 new Run");
  });
});

describe("authorSlot", () => {
  it("a Run with no author, a schedule included, gets a muted dash", () => {
    expect(authorSlot("cron", null)).toEqual({ text: "—", muted: true });
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

describe("rowStatus", () => {
  it("a failed Run is one word; its card names the first failed step, in Mono, and how many more failed", () => {
    expect(rowStatus({ status: "failed", failed_step: "test[os=macos]", failed_steps_count: 2 }, null)).toEqual({
      word: "Failed",
      tone: "failed",
      detail: {
        parts: [
          { t: "text", text: "failed at " },
          { t: "mono", text: "test · macos +1" },
        ],
        text: "failed at test · macos +1",
        tone: "failed",
      },
    });
    const dead = rowStatus({ status: "dead_lettered", failed_step: "migrate", failed_steps_count: 1 }, null);
    expect([dead.word, dead.detail?.text]).toEqual(["Dead lettered", "failed at migrate"]);
  });

  it("a suspended Run reads Waiting; its card says what it waits for", () => {
    const approval = rowStatus({ status: "suspended", waiting_for: { step: "approve", kind: "environment_approval" } }, null);
    expect([approval.word, approval.detail?.text, approval.detail?.tone]).toEqual(["Waiting", "waiting for approval", "waiting"]);
    expect(rowStatus({ status: "suspended", waiting_for: { step: "soak", kind: "timer" } }, null).detail?.text).toBe("waiting for a timer");
  });

  it("a replaced Run reads Replaced; its card names the replacer, in Mono, when it is on the page", () => {
    const r = rowStatus({ status: "cancelled" }, { number: 130 });
    expect([r.word, r.tone, r.detail?.text]).toEqual(["Replaced", "cancelled", "replaced by #130, a newer push"]);
    expect(r.detail?.parts.filter((p) => p.t === "mono").map((p) => p.text)).toEqual(["#130"]);
    expect(rowStatus({ status: "cancelled" }, { number: null }).detail?.text).toBe("replaced by a newer push");
  });

  it("other Runs, and an older server's rows without the new fields, have no card", () => {
    expect(rowStatus({ status: "failed" }, null)).toEqual({ word: "Failed", tone: "failed", detail: null });
    expect(rowStatus({ status: "suspended" }, null)).toEqual({ word: "Waiting", tone: "suspended", detail: null });
    expect(rowStatus({ status: "running" }, null).word).toBe("Running");
    expect(rowStatus({ status: "pending" }, null).word).toBe("Pending");
    expect(rowStatus({ status: "cancelled" }, null).word).toBe("Cancelled");
    expect(rowStatus({ status: "succeeded" }, null)).toEqual({ word: "Passed", tone: "succeeded", detail: null });
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
