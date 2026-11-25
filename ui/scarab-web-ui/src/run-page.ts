// Pure helpers behind the Run page: matrix step labels, the summary line,
// which step opens first, the URL query state, the graph node's meta line,
// the graph band's fit, and the step header's status words and status line.
// No DOM, so the vitest suite covers them (run-page.test.ts).
import { isGateOpen } from "./gates";
import { duration } from "./fmt";

// ── Matrix step ids ─────────────────────────────────────────────────────────

/** A matrix cell's id split into its step and its coordinate. */
export type MatrixId = { base: string; keys: string[]; values: string[] };

const KEY_RE = /^[A-Za-z_][A-Za-z0-9_-]*$/;

/** Whether every `[` in `s` is closed by a later `]`. */
function balanced(s: string): boolean {
  let depth = 0;
  for (const c of s) {
    if (c === "[") depth++;
    if (c === "]") depth--;
    if (depth < 0) return false;
  }
  return depth === 0;
}

/**
 * Split a matrix cell's id, `base[k=v,k=v]`, into its step and coordinate.
 *
 * The compiler writes the coordinate as `k=v` pairs joined by `,` and does not
 * escape values, so a value that holds `,`, `=`, `[` or `]` makes the split a
 * guess. Every such case returns null and the caller shows the full id. The
 * last `[…]` is the coordinate, so a step inlined under a matrixed invoke
 * (`ship[svc=api]/probe[n=1]`) keeps the invoke part in its base.
 */
export function parseMatrixId(id: string): MatrixId | null {
  if (!id.endsWith("]")) return null;
  const open = id.lastIndexOf("[");
  if (open <= 0) return null;
  const base = id.slice(0, open);
  const inner = id.slice(open + 1, -1);
  if (!inner || inner.includes("]") || !balanced(base)) return null;
  const keys: string[] = [];
  const values: string[] = [];
  for (const part of inner.split(",")) {
    const eq = part.indexOf("=");
    if (eq <= 0 || part.indexOf("=", eq + 1) !== -1) return null;
    const k = part.slice(0, eq);
    const v = part.slice(eq + 1);
    if (!KEY_RE.test(k) || !v) return null;
    keys.push(k);
    values.push(v);
  }
  if (new Set(keys).size !== keys.length) return null;
  return { base, keys, values };
}

/** The heading over a run of matrix cells: "plan · matrix · cluster · phase". */
export function matrixHeading(m: { base: string; keys: string[] }): string {
  return `${m.base} · matrix · ${m.keys.join(" · ")}`;
}

/** A run of consecutive items: the cells of one matrix (with its heading), or
 * a single item that is not a matrix cell. */
export type MatrixSegment<T> = {
  matrix: { base: string; keys: string[] } | null;
  items: { item: T; label: string }[];
};

/**
 * Group consecutive items that are cells of the same matrix, so each group can
 * sit under one heading and each cell can show only its values. A cell joins
 * the group before it only when both name the same step and the same keys in
 * the same order; anything else starts a new segment. An item whose id is null
 * (an invocation box) or does not parse is a segment of its own, labelled with
 * its full id.
 */
export function segmentByMatrix<T>(items: T[], idOf: (t: T) => string | null): MatrixSegment<T>[] {
  const out: MatrixSegment<T>[] = [];
  for (const item of items) {
    const id = idOf(item);
    const m = id === null ? null : parseMatrixId(id);
    const last = out[out.length - 1];
    if (
      m &&
      last?.matrix &&
      last.matrix.base === m.base &&
      last.matrix.keys.join("\u0000") === m.keys.join("\u0000")
    ) {
      last.items.push({ item, label: m.values.join(" · ") });
      continue;
    }
    out.push({
      matrix: m ? { base: m.base, keys: m.keys } : null,
      items: [{ item, label: m ? m.values.join(" · ") : (id ?? "") }],
    });
  }
  return out;
}

/** The step pane's name for a step: the base step and, for a matrix cell, its
 * values ("plan" and "treebo-production · aws-cluster"). */
export function stepName(id: string): { base: string; cell: string | null } {
  const m = parseMatrixId(id);
  return m ? { base: m.base, cell: m.values.join(" · ") } : { base: id, cell: null };
}

/** A step's name on one line: "plan · treebo-production · aws-cluster". */
export function stepDisplayName(id: string): string {
  const n = stepName(id);
  return n.cell ? `${n.base} · ${n.cell}` : n.base;
}

// ── Status words and glyphs ─────────────────────────────────────────────────

/** One glyph per step or try status, used by the summary, list and header. */
export function statusGlyph(status: string): string {
  switch (status) {
    case "succeeded":
      return "✓";
    case "failed":
      return "✕";
    case "running":
      return "●";
    case "held":
      return "◷";
    case "superseded":
    case "cancelled":
      return "⊘";
    case "skipped":
      return "↷";
    default:
      return "○";
  }
}

/** A step status in words: "Passed", "Failed", "Not run". */
export function statusWord(status: string): string {
  switch (status) {
    case "succeeded":
      return "Passed";
    case "failed":
      return "Failed";
    case "running":
      return "Running";
    case "not_run":
      return "Not run";
    case "":
      return "Pending";
  }
  return status.charAt(0).toUpperCase() + status.slice(1).replace(/_/g, " ");
}

/** A failure class (`AttemptDto.failure`) in words. */
export function failureWords(failure: string | null | undefined): string {
  switch (failure) {
    case "step":
      return "step error";
    case "infra":
      return "infrastructure error";
    case "timeout":
      return "timed out";
    case "lost":
      return "Pod lost";
    case "config":
      return "config error";
    case "missing-inputs":
      return "missing inputs";
    case "gate":
      return "gate failed";
    case null:
    case undefined:
    case "":
      return "failed";
  }
  return failure;
}

/** A compact duration for the graph: `4.2s`, `1m12s`, `1h03m`. */
export function compactDuration(ms: number): string {
  const s = Math.max(0, ms) / 1000;
  if (s < 10) return `${s.toFixed(1)}s`;
  const total = Math.round(s);
  if (total < 60) return `${total}s`;
  const mm = Math.floor(total / 60);
  const ss = total % 60;
  if (mm < 60) return `${mm}m${String(ss).padStart(2, "0")}s`;
  const hh = Math.floor(mm / 60);
  return `${hh}h${String(mm % 60).padStart(2, "0")}m`;
}

/** A rough duration: "about 40s", "about 3m", "about 2h". */
export function aboutDuration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `about ${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `about ${m}m`;
  return `about ${Math.round(m / 60)}h`;
}

// ── Summary line and root failures ──────────────────────────────────────────

export type StepLike = { id: string; status: string; needs?: string[] | null };

/**
 * The failed steps whose prerequisites all passed: where a failure started,
 * as opposed to a step that failed because something before it did. A need the
 * run does not list counts as passed.
 */
export function rootFailures(steps: StepLike[]): string[] {
  const by = new Map(steps.map((s) => [s.id, s.status]));
  return steps
    .filter(
      (s) => s.status === "failed" && (s.needs ?? []).every((n) => (by.get(n) ?? "succeeded") === "succeeded"),
    )
    .map((s) => s.id);
}

export type RunSummary = {
  total: number;
  failed: number;
  /** Root failures in run order (see `rootFailures`). */
  roots: string[];
  /** Running steps that no infra condition holds back. */
  running: number;
  passed: number;
  /** Running steps an active infra condition holds back, by what they wait
   * for ("waiting for a node"), in first-seen order. */
  held: { text: string; count: number }[];
};

/** The counts the summary line above the graph reads. `held` maps a step id to
 * the words for its active infra condition. */
export function runSummary(steps: StepLike[], held: Record<string, string>): RunSummary {
  const heldCounts = new Map<string, number>();
  let running = 0;
  for (const s of steps) {
    if (s.status !== "running") continue;
    const h = held[s.id];
    if (h) heldCounts.set(h, (heldCounts.get(h) ?? 0) + 1);
    else running++;
  }
  return {
    total: steps.length,
    failed: steps.filter((s) => s.status === "failed").length,
    roots: rootFailures(steps),
    running,
    passed: steps.filter((s) => s.status === "succeeded").length,
    held: [...heldCounts].map(([text, count]) => ({ text, count })),
  };
}

// ── Default selection ───────────────────────────────────────────────────────

/**
 * The step the page opens on when the URL names none: the first gate waiting
 * for a decision, else the first root failure, else the first step an infra
 * condition holds back, else the first running step, else the last finished
 * one, else the first step. Null for a run with no steps.
 */
export function defaultSelection(o: {
  steps: StepLike[];
  /** Gates a person can act on now, in run order. */
  gates: string[];
  /** Steps an active infra condition holds back. */
  held: string[];
}): string | null {
  if (!o.steps.length) return null;
  if (o.gates.length) return o.gates[0];
  const roots = rootFailures(o.steps);
  if (roots.length) return roots[0];
  const held = new Set(o.held);
  const firstHeld = o.steps.find((s) => held.has(s.id));
  if (firstHeld) return firstHeld.id;
  const running = o.steps.find((s) => s.status === "running");
  if (running) return running.id;
  const finished = o.steps.filter((s) => s.status === "succeeded" || s.status === "failed");
  if (finished.length) return finished[finished.length - 1].id;
  return o.steps[0].id;
}

// ── URL state ───────────────────────────────────────────────────────────────

export type RunTabParam = "artifacts" | "activity" | "inputs" | "manifest";
export type PaneTabParam = "results" | "workspace" | "outputs" | "source";

/**
 * The run page's query state. Steps and services are named by their index in
 * the run's lists, never by id: URLs carry no readable names.
 * - `step`: 0-based index into the run's steps.
 * - `service`: 0-based index into the run's shared services.
 * - `v`: the version in view, only when it is not the latest.
 * - `try`: the try number within that version, only when the user picked one.
 * - `tab`: the run tab (absent = pipeline); `ptab`: the step tab (absent = logs).
 * - `from`: 0-based index into the run's steps whose files the Artifacts tab
 *   shows; only with `tab=artifacts`.
 */
export type RunUrlState = {
  step?: number;
  service?: number;
  v?: number;
  try?: number;
  tab?: RunTabParam;
  ptab?: PaneTabParam;
  from?: number;
};

type Params = Record<string, string | string[] | null | undefined>;

const RUN_TABS: readonly string[] = ["artifacts", "activity", "inputs", "manifest"];
const PANE_TABS: readonly string[] = ["results", "workspace", "outputs", "source"];

function first(v: string | string[] | null | undefined): string | undefined {
  if (Array.isArray(v)) return v[0];
  return v ?? undefined;
}

function nat(v: string | undefined, min: number): number | undefined {
  if (v === undefined || !/^\d{1,9}$/.test(v)) return undefined;
  const n = Number(v);
  return n >= min ? n : undefined;
}

/** Read the query params, keeping only well-formed values. Whether an index
 * exists in this run is `clampRunUrl`'s job. */
export function parseRunUrl(params: Params): RunUrlState {
  const out: RunUrlState = {};
  const step = nat(first(params.step), 0);
  const service = nat(first(params.service), 0);
  const v = nat(first(params.v), 1);
  const tryN = nat(first(params.try), 1);
  const tab = first(params.tab);
  const ptab = first(params.ptab);
  const from = nat(first(params.from), 0);
  if (step !== undefined) out.step = step;
  if (service !== undefined) out.service = service;
  if (v !== undefined) out.v = v;
  if (tryN !== undefined) out.try = tryN;
  if (tab && RUN_TABS.includes(tab)) out.tab = tab as RunTabParam;
  if (ptab && PANE_TABS.includes(ptab)) out.ptab = ptab as PaneTabParam;
  if (from !== undefined && out.tab === "artifacts") out.from = from;
  return out;
}

/**
 * Drop every param that does not exist in this run: a step or service index
 * past the end, a version past the last (or the latest itself, which is the
 * absence of `v`), a try past the version's tries, an Artifacts step filter
 * past the end or without the Artifacts tab. A step wins over a service
 * when both are given. `services` null means the list has not loaded, so a
 * service index is kept for now.
 */
export function clampRunUrl(
  s: RunUrlState,
  ctx: {
    steps: number;
    services: number | null;
    versions: number;
    /** How many tries step `step` has in version `version`. */
    triesOf: (step: number, version: number) => number;
  },
): RunUrlState {
  const out: RunUrlState = {};
  if (s.tab) out.tab = s.tab;
  if (s.ptab) out.ptab = s.ptab;
  if (s.from !== undefined && s.tab === "artifacts" && s.from < ctx.steps) out.from = s.from;
  if (s.v !== undefined && s.v < ctx.versions) out.v = s.v;
  if (s.step !== undefined && s.step < ctx.steps) {
    out.step = s.step;
    if (s.try !== undefined && s.try <= ctx.triesOf(s.step, out.v ?? ctx.versions)) out.try = s.try;
  } else if (s.service !== undefined && (ctx.services === null || s.service < ctx.services)) {
    out.service = s.service;
  }
  return out;
}

/** The state as query params for a merge: every key present, `undefined` for
 * one to remove. */
export function runUrlParams(s: RunUrlState): Record<string, string | undefined> {
  const str = (n: number | undefined) => (n === undefined ? undefined : String(n));
  return {
    step: str(s.step),
    service: s.step === undefined ? str(s.service) : undefined,
    v: str(s.v),
    try: s.step === undefined ? undefined : str(s.try),
    tab: s.tab,
    ptab: s.ptab,
    from: s.tab === "artifacts" ? str(s.from) : undefined,
  };
}

// ── Graph node meta ─────────────────────────────────────────────────────────

export type MetaTone = "passed" | "failed" | "waiting" | "warning";

export type NodeMetaInput = {
  status: string;
  /** Tries in the version in view. */
  tries: number;
  /** Of those, the ones that failed. */
  failedTries?: number;
  durationMs?: number | null;
  runningSince?: number | null;
  /** A gate verdict, a hold, or (tone `warning`) an infra condition holding
   * the step back, with when it started. */
  note?: { tone: MetaTone; text: string; since?: number | null } | null;
  gate?: string | null;
  approvalGate?: boolean;
};

/** The line under a graph node's name. */
export function nodeMeta(s: NodeMetaInput, now: number): { text: string; tone: MetaTone | null } {
  if (s.note?.tone === "warning") {
    const since = s.note.since;
    return { text: since != null ? `${s.note.text} · ${compactDuration(now - since)}` : s.note.text, tone: "warning" };
  }
  if (s.status === "running") {
    return { text: s.runningSince ? `running · ${compactDuration(now - s.runningSince)}` : "running", tone: null };
  }
  if (s.note) return { text: s.note.text, tone: s.note.tone };
  if (isGateOpen({ gate: s.gate, status: s.status })) {
    return { text: s.approvalGate ? "gate · approval" : `gate · ${s.gate}`, tone: null };
  }
  if (s.status === "succeeded" || s.status === "failed") {
    const d = s.durationMs != null ? ` · ${compactDuration(s.durationMs)}` : "";
    if (s.status === "succeeded") {
      return { text: s.tries > 1 ? `passed on try ${s.tries}${d}` : `passed${d}`, tone: null };
    }
    const failed = s.failedTries ?? s.tries;
    return {
      text: s.tries > 1 ? `failed ${failed} of ${s.tries} tries${d}` : `failed${d}`,
      tone: "failed",
    };
  }
  if (s.status === "superseded") return { text: "superseded", tone: null };
  if (s.status === "not_run") return { text: "not run", tone: null };
  return { text: s.status, tone: null };
}

// ── Graph band fit ──────────────────────────────────────────────────────────

/**
 * How the graph band draws its content. The graph is drawn at 100% and never
 * shrunk to fit the height: the band is as tall as the graph, up to `maxH`,
 * and scrolls inside past that. A graph wider than the band is not shrunk
 * either (`minScale` defaults to 1, so the step names stay at the Mono size):
 * it scrolls (and pans) sideways, and the zoom buttons zoom out.
 */
export function fitBand(o: {
  bandW: number;
  contentW: number;
  contentH: number;
  maxH: number;
  pad: number;
  minScale?: number;
}): { scale: number; bandH: number; pannable: boolean } {
  const min = o.minScale ?? 1;
  const avail = Math.max(1, o.bandW - o.pad * 2);
  const scale = o.contentW <= avail ? 1 : Math.max(min, avail / o.contentW);
  const fullH = Math.round(o.contentH * scale + o.pad * 2);
  const bandH = Math.min(fullH, o.maxH);
  return { scale, bandH, pannable: o.contentW * scale > avail + 0.5 || fullH > o.maxH };
}

// ── Graph node width ────────────────────────────────────────────────────────

/**
 * The width of the nodes in one graph column: wide enough for the column's
 * longest label on one line, so a label ends in an ellipsis only past `max`.
 * Labels are in the 12px mono font, `charPx` a character (JetBrains Mono's
 * advance is 0.6em); `chromePx` is the node's padding, border, status ring,
 * gaps and gate icon. `minChars` keeps room for the meta line under the label
 * ("waiting for a node · 1m10s"), which changes as the run goes and so does
 * not size the node.
 */
export function columnNodeWidth(
  labels: string[],
  o: { charPx?: number; chromePx?: number; minChars?: number; min?: number; max?: number } = {},
): number {
  const charPx = o.charPx ?? 7.2;
  const chrome = o.chromePx ?? 68;
  const longest = labels.reduce((n, l) => Math.max(n, [...l].length), o.minChars ?? 0);
  return Math.round(Math.min(o.max ?? 460, Math.max(o.min ?? 196, longest * charPx + chrome)));
}

// ── Step header ─────────────────────────────────────────────────────────────

export type TryLike = {
  outcome: string;
  failure?: string | null;
  startedAt?: number | null;
  finishedAt?: number | null;
};

/** The step header's status words for the selected try: "✓ Passed in 2m11s",
 * "✕ Failed after 2m06s: step error", "● Running for 1m12s", or, while an
 * infra condition holds the try back, "◷ Waiting for a node". */
export function tryStatusWords(
  t: TryLike | null,
  now: number,
  held?: string | null,
): { text: string; tone: "passed" | "failed" | "running" | "warning" | "muted" } {
  if (!t) return { text: "○ Not run in this version", tone: "muted" };
  if (held) return { text: `◷ ${held.charAt(0).toUpperCase()}${held.slice(1)}`, tone: "warning" };
  const took = t.startedAt != null && t.finishedAt != null ? duration(t.startedAt, t.finishedAt) : null;
  switch (t.outcome) {
    case "running":
      return {
        text: t.startedAt != null ? `● Running for ${duration(t.startedAt, now)}` : "● Running",
        tone: "running",
      };
    case "succeeded":
      return { text: took ? `✓ Passed in ${took}` : "✓ Passed", tone: "passed" };
    case "failed":
      return {
        text: `✕ Failed${took ? ` after ${took}` : ""}: ${failureWords(t.failure)}`,
        tone: "failed",
      };
    case "superseded":
      return { text: `⊘ Superseded${took ? ` after ${took}` : ""}`, tone: "muted" };
    case "cancelled":
      return { text: `⊘ Cancelled${took ? ` after ${took}` : ""}`, tone: "muted" };
  }
  return { text: statusWord(t.outcome), tone: "muted" };
}

/** What the status line under the step name adds after the selected try's
 * status ("✕ Failed after 2m06s: step error"): why it failed or what holds it
 * back (`reason`), how long it waited for a node before it started
 * (`waited`), and, when no try ran in this version, the earlier version's try
 * the step was reused from (`reused`). Empty when there is nothing to add. */
export type TryLine = {
  reason?: string;
  waited?: string;
  reused?: { take: number; try: number };
};

export function tryLine(o: {
  tries: { outcome: string; failureDetail?: string | null }[];
  /** Index of the selected try; null when nothing ran in this version. */
  selected: number | null;
  /** How long a closed condition kept the selected try from starting. */
  heldBeforeMs?: number | null;
  /** The condition holding the selected try back now. */
  held?: { text: string; message?: string } | null;
  /** The earlier version's try a not-run step was reused from. */
  carried?: { take: number; try: number } | null;
}): TryLine {
  if (o.selected === null) return o.carried ? { reused: o.carried } : {};
  const t = o.tries[o.selected];
  if (!t) return {};
  if (o.held) return o.held.message ? { reason: o.held.message } : {};
  const line: TryLine = {};
  if (t.outcome === "failed" && t.failureDetail) line.reason = t.failureDetail;
  if (o.heldBeforeMs) line.waited = `waited ${aboutDuration(o.heldBeforeMs)} for a node`;
  return line;
}

/** "Not the latest try: try 3 is running", when the selected try is not the
 * newest; null when it is. */
export function notLatestNote(
  tries: { outcome: string }[],
  selected: number | null,
  noun = "Try",
): { text: string; latest: number } | null {
  if (selected === null || selected >= tries.length - 1) return null;
  const latest = tries.length - 1;
  const o = tries[latest].outcome;
  const what =
    o === "running"
      ? "is running"
      : o === "succeeded"
        ? "passed"
        : o === "failed"
          ? "failed"
          : `is ${statusWord(o).toLowerCase()}`;
  return { text: `Not the latest ${noun.toLowerCase()}: ${noun.toLowerCase()} ${latest + 1} ${what}`, latest };
}
