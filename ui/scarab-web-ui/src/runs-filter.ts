// The Runs tab's server-side filters: Environment, and the
// deploy marker (a pipeline or Step marked `deploy: true`). Both run on the
// server, so the rows on screen are already filtered — the Environment
// picker's options can't come from them alone, or picking one Environment
// would hide every other.

import { isScheduled } from "./schedules";

/** The Environment picker's options: the Project's Environments, every
 *  Environment a loaded run is governed by (a run can outlive the Project's
 *  eligibility for its Environment), and the selected one (a shared link can
 *  name an Environment neither list has). Distinct, non-empty, sorted. */
export function runEnvironmentOptions(
  projectEnvironments: readonly string[],
  runs: readonly { environment?: string | null }[],
  selected: string,
): string[] {
  const seen = new Set<string>();
  for (const e of projectEnvironments) if (e) seen.add(e);
  for (const r of runs) if (r.environment) seen.add(r.environment);
  if (selected) seen.add(selected);
  return [...seen].sort();
}

/** The runs request's server-side filter: the selected Environment (omitted
 *  when none — never `""`, which the server rejects with a 400), and
 *  `deploy: true` when the Deploys toggle is on (omitted otherwise, so the
 *  list is every run rather than only the non-deploys). */
export function runListFilter(
  environment: string,
  deploysOnly: boolean,
): { environment?: string; deploy?: true } {
  return {
    ...(environment ? { environment } : {}),
    ...(deploysOnly ? { deploy: true as const } : {}),
  };
}

/** A row of the Runs list: a Run, or a trigger that was refused before it
 *  became one. */
export type RunListRow<R, F> = { kind: "run"; run: R; at: number } | { kind: "refused"; refused: F; at: number };

/** The bare pipeline name of a refused Run's file (`.scarab/ci.yaml` → `ci`),
 *  the name a Run's `pipeline` carries. */
export function barePipeline(path: string): string {
  return path.replace(/^\.scarab\//, "").replace(/\.ya?ml$/, "");
}

/** The Runs list with refused triggers between the Runs, newest first.
 *
 *  - `refused` status shows only refusals; `running` and `failed` only Runs.
 *  - A refusal older than the oldest loaded Run is left out: the page shows
 *    a window of time, and an old refusal at its foot would read as recent.
 *  - The author, pipeline and scheduled filters apply to refusals too (a
 *    schedule's missed slot is a refused `cron` trigger); a filter a refusal
 *    cannot match (an Environment, deploys, a PR) leaves them out. */
export function mergeRefused<
  R extends { created_at: number },
  F extends { created_at: number; actor?: string | null; pipeline: string; trigger_kind?: string | null },
>(
  runs: readonly R[],
  refused: readonly F[],
  f: { status: string; author: string; pipeline: string; runOnly: boolean; scheduled?: boolean },
): RunListRow<R, F>[] {
  const runRows: RunListRow<R, F>[] =
    f.status === "refused" ? [] : runs.map((run) => ({ kind: "run", run, at: run.created_at }));
  const oldest = runs.length ? Math.min(...runs.map((r) => r.created_at)) : -Infinity;
  const keepRefused = (f.status === "all" || f.status === "refused") && !f.runOnly;
  const refusedRows: RunListRow<R, F>[] = keepRefused
    ? refused
        .filter((r) => f.status === "refused" || r.created_at >= oldest)
        .filter((r) => !f.author || r.actor === f.author)
        .filter((r) => !f.pipeline || barePipeline(r.pipeline) === f.pipeline)
        .filter((r) => !f.scheduled || isScheduled(r.trigger_kind))
        .map((refused) => ({ kind: "refused", refused, at: refused.created_at }))
    : [];
  return [...runRows, ...refusedRows].sort((a, b) => b.at - a.at);
}

// ── The status segments ────────────────────────────────────────────────────

/** The segmented control over the list: `?status=` in the URL. */
export type StatusSegment = "all" | "running" | "waiting" | "failed" | "refused";

export const SEGMENTS: readonly [StatusSegment, string][] = [
  ["all", "All"],
  ["running", "Running"],
  ["waiting", "Waiting"],
  ["failed", "Failed"],
  ["refused", "Refused"],
];

/** `?status=` as a segment; anything else (absent, hand-edited) is `all`. */
export function parseSegment(v: string): StatusSegment {
  return SEGMENTS.some(([k]) => k === v) ? (v as StatusSegment) : "all";
}

/** Whether a Run's status belongs in a segment. Running takes the pending
 *  Runs too, Waiting the suspended ones (held at a gate), Failed the
 *  dead-lettered ones. No Run is in Refused: a refused trigger never became
 *  one (`mergeRefused` adds those). */
export function inSegment(status: string, seg: StatusSegment): boolean {
  switch (seg) {
    case "all":
      return true;
    case "running":
      return status === "running" || status === "pending";
    case "waiting":
      return status === "suspended";
    case "failed":
      return status === "failed" || status === "dead_lettered";
    case "refused":
      return false;
  }
}

/** The filters besides the status, as `mergeRefused` reads them. */
export type RowFilter = { author: string; pipeline: string; runOnly: boolean; scheduled?: boolean };

/** The rows a segment shows: `runs` (already through every filter but the
 *  status) narrowed to the segment, with the refused triggers it keeps. */
export function segmentRows<
  R extends { created_at: number; status: string },
  F extends { created_at: number; actor?: string | null; pipeline: string; trigger_kind?: string | null },
>(runs: readonly R[], refused: readonly F[], seg: StatusSegment, f: RowFilter): RunListRow<R, F>[] {
  return mergeRefused(
    runs.filter((r) => inSegment(r.status, seg)),
    refused,
    { ...f, status: seg },
  );
}

/** Each segment's count: the rows it would show under the other filters, so
 *  a count is what a click on it lists. Over the loaded rows only. */
export function segmentCounts<
  R extends { created_at: number; status: string },
  F extends { created_at: number; actor?: string | null; pipeline: string; trigger_kind?: string | null },
>(runs: readonly R[], refused: readonly F[], f: RowFilter): Record<StatusSegment, number> {
  const out = {} as Record<StatusSegment, number>;
  for (const [seg] of SEGMENTS) out[seg] = segmentRows(runs, refused, seg, f).length;
  return out;
}

// ── Active filters ─────────────────────────────────────────────────────────

/** The filters the Filter panel sets, as their URL params hold them. */
export type PanelFilters = {
  pipeline: string;
  env: string;
  author: string;
  branch: string;
  pr: string;
  deploy: boolean;
  scheduled: boolean;
};

/** One removable chip under the toolbar: `pipeline deploy-preview…`, or a
 *  switch with no value (`deploys only`). `param` is the search param that
 *  removing it clears; `full` the uncut value, for the chip's title. */
export type FilterChip = { param: string; key: string; value: string | null; full: string | null };

/** A filter value cut to 28 characters with "…" inside the count. */
function cutValue(v: string): string {
  const chars = [...v];
  return chars.length <= 28 ? v : chars.slice(0, 27).join("") + "…";
}

/** The chips for the filters that are on, in the panel's order. Values are
 *  cut to 28 characters (they are set in Mono). */
export function filterChips(f: PanelFilters): FilterChip[] {
  const out: FilterChip[] = [];
  const add = (param: string, key: string, v: string) => {
    if (v) out.push({ param, key, value: cutValue(v), full: v });
  };
  add("pipeline", "pipeline", f.pipeline);
  add("env", "environment", f.env);
  add("author", "author", f.author);
  add("branch", "branch", f.branch);
  if (f.pr) out.push({ param: "pr", key: "pr", value: `#${f.pr}`, full: `#${f.pr}` });
  if (f.deploy) out.push({ param: "deploy", key: "deploys only", value: null, full: null });
  if (f.scheduled) out.push({ param: "scheduled", key: "scheduled only", value: null, full: null });
  return out;
}

/** A piece of the no-match sentence: words, or a value set in Mono. */
export type SentencePart = { t: "text"; text: string } | { t: "mono"; text: string };

/** The no-match sentence: what was asked for, over how many loaded Runs.
 *  `No failed deploy Run of deploy-preview… by j.rao in the latest 50 Runs.`
 *  When the server's filters (an Environment, deploys) left no Run at all,
 *  there is no window to name: `No deploy Run into production.` */
export function noMatchSentence(f: PanelFilters, seg: StatusSegment, loaded: number): SentencePart[] {
  const words: string[] = [];
  if (seg !== "all" && seg !== "refused") words.push(seg);
  if (f.scheduled) words.push("scheduled");
  if (f.deploy) words.push("deploy");
  words.push(seg === "refused" ? "refused trigger" : "Run");
  const parts: SentencePart[] = [{ t: "text", text: `No ${words.join(" ")}` }];
  const add = (lead: string, v: string) => {
    if (v) parts.push({ t: "text", text: ` ${lead} ` }, { t: "mono", text: cutValue(v) });
  };
  add("of", f.pipeline);
  add("into", f.env);
  add("by", f.author);
  add("on", f.branch);
  if (f.pr) add("for PR", `#${f.pr}`);
  parts.push({ t: "text", text: loaded > 0 ? ` in the latest ${loaded} ${loaded === 1 ? "Run" : "Runs"}.` : "." });
  return parts;
}
