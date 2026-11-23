// One row of the Runs list, as words: the status mark, the headline, the
// phrase that says where a Run came from, the status phrase, and the order and
// day groups of the list. Pure, so the list and its tests agree without a DOM.
import { statusWord, stepDisplayName } from "./run-page";

/** The glyph inside a row's status mark. `null` draws the mark with no glyph:
 *  a spinning ring for a running Run, a dashed ring for a pending one. */
export function statusGlyph(status: string): string | null {
  switch (status) {
    case "succeeded":
      return "check";
    case "failed":
      return "x";
    case "dead_lettered":
      return "exclamation";
    case "cancelled":
      return "minus";
    case "suspended":
      return "pause";
    case "refused":
      return "ban";
  }
  return null;
}

/** A branch or tag ref without its `refs/heads/` or `refs/tags/` prefix. */
export function shortRef(ref: string | null | undefined): string | null {
  if (!ref) return null;
  return ref.replace(/^refs\/(heads|tags)\//, "");
}

/** The pull request number in a `refs/pull/<n>/head` ref, which is what a
 *  pull_request Run records as its ref. */
export function pullNumber(ref: string | null | undefined): number | null {
  const m = /^refs\/pull\/(\d+)\//.exec(ref ?? "");
  return m ? Number(m[1]) : null;
}

/** Line 1's headline. `muted` marks a stand-in for a Run that has none. */
export function runHeadline(kind: string | null | undefined, title: string | null | undefined): { text: string; muted: boolean } {
  // A cron Run's title is its expression, which line 2 shows.
  if (kind === "cron") return { text: "Scheduled run", muted: false };
  if (title) return { text: title, muted: false };
  return { text: kind === "api" ? "Started from the API" : "No headline", muted: true };
}

/** What the phrase is built from. */
export type Origin = {
  kind?: string | null;
  /** The ref as the server records it (`refs/heads/main`, `refs/pull/7/head`). */
  ref?: string | null;
  pr?: number | null;
  /** A pull request's base branch. */
  base?: string | null;
  /** A cron Run's title: the expression that fired. */
  title?: string | null;
};

/** One piece of the phrase. `pr` is the forge link; `arrow` carries its own
 *  spaces when drawn (` → `). */
export type PhrasePart =
  | { t: "text"; text: string }
  | { t: "mono"; text: string }
  | { t: "muted"; text: string }
  | { t: "arrow" }
  | { t: "pr"; n: number };

/** Where a Run came from, without the commit and the author (line 2 shows
 *  those in their own places): `main`, `#212 → main`, `tag v0.7.2`,
 *  `schedule 0 2 * * * on main`, `manual on feat/x`. */
export function wherePhrase(o: Origin): PhrasePart[] {
  const ref = o.ref ?? null;
  const branch = ref && !ref.startsWith("refs/pull/") ? shortRef(ref) : null;
  const on = (lead: string): PhrasePart[] =>
    branch ? [{ t: "text", text: `${lead} on ` }, { t: "mono", text: branch }] : [{ t: "text", text: lead }];
  switch (o.kind) {
    case null:
    case undefined:
    case "":
      return [{ t: "muted", text: "no trigger recorded" }];
    case "push":
      return branch ? [{ t: "mono", text: branch }] : [{ t: "text", text: "push" }];
    case "pull_request": {
      const n = o.pr ?? pullNumber(ref);
      const what: PhrasePart[] = n != null ? [{ t: "pr", n }] : [];
      // The head branch only when the ref names one: a pull_request Run
      // records `refs/pull/<n>/head`, which says nothing `#n` does not.
      if (branch) {
        if (what.length > 0) what.push({ t: "text", text: " " });
        what.push({ t: "mono", text: branch });
      }
      if (o.base) what.push({ t: "arrow" }, { t: "mono", text: o.base });
      return what.length > 0 ? what : [{ t: "text", text: "pull request" }];
    }
    case "tag":
    case "release":
      return branch ? [{ t: "text", text: `${o.kind} ` }, { t: "mono", text: branch }] : [{ t: "text", text: o.kind }];
    case "cron": {
      const what: PhrasePart[] = [{ t: "text", text: "schedule" }];
      if (o.title) what.push({ t: "text", text: " " }, { t: "mono", text: o.title });
      if (branch) what.push({ t: "text", text: " on " }, { t: "mono", text: branch });
      return what;
    }
    case "manual":
      return on("manual");
    case "api":
      return on("API");
    default:
      return on(o.kind);
  }
}

/** The author's place on line 2: the Actor, or a muted stand-in. A schedule
 *  has no author, so it reads `schedule`. */
export function authorSlot(kind: string | null | undefined, actor: string | null | undefined): { text: string; muted: boolean } {
  if (actor) return { text: actor, muted: false };
  return { text: kind === "cron" ? "schedule" : "—", muted: true };
}

/** A name cut to `max` characters with "…" at the end. Pipeline and
 *  Environment names are set in Mono, so a character count is a width, and
 *  the cut leaves no gap before what follows (a CSS ellipsis does). */
export function cutName(name: string, max = 24): string {
  const chars = [...name];
  return chars.length <= max ? name : chars.slice(0, max - 1).join("") + "…";
}

/** The phrase as plain text, as it reads on screen. */
export function phraseText(parts: PhrasePart[]): string {
  return parts
    .map((p) => {
      switch (p.t) {
        case "arrow":
          return " → ";
        case "pr":
          return `#${p.n}`;
        default:
          return p.text;
      }
    })
    .join("");
}

// ── The status phrase ──────────────────────────────────────────────────────

/** The runs list fields this file reads beyond the generated `RunSummaryDto`
 *  today. Waiting on the server field: `replaced_by`, `failed_step`,
 *  `failed_steps_count` and `waiting_for` are being added to the runs list
 *  response; once the generated schema has them, this type goes. Each is
 *  optional, so an older server's rows read as plain status words. */
export type RunRowFields = {
  /** The Run that took this one's place (a newer push to the same pull
   *  request or branch). */
  replaced_by?: string | null;
  /** A failed or dead-lettered Run's first failed step id. */
  failed_step?: string | null;
  /** How many steps failed, `failed_step` included. */
  failed_steps_count?: number | null;
  /** The gate a suspended Run is held at. */
  waiting_for?: { step: string; kind: string } | null;
};

/** What a Run waits for, by gate kind: "approval", "a timer". */
export function waitingWord(kind: string): string {
  switch (kind) {
    case "manual":
    case "environment_approval":
      return "approval";
    case "timer":
      return "a timer";
    case "external":
      return "an external check";
    case "plugin":
      return "a plugin";
  }
  return kind.replace(/_/g, " ");
}

/** The right column's status phrase. `lead` is set in Label and `step` (a
 *  step name) in Mono after it; `tone` is the status whose colour it takes. */
export type StatusPhrase = { lead: string; step: string | null; tone: string };

/** How a Run was replaced, for its status phrase: `number` is the replacing
 *  Run's `#N` when that Run is on the page, else `null`. */
export type Replaced = { number: number | null };

/** A Run's status in words: `Failed at test · macos +1`, `Waiting for
 *  approval`, `Replaced by #130`, else the plain status word. */
export function statusPhrase(
  r: { status: string } & RunRowFields,
  replaced: Replaced | null,
): StatusPhrase {
  if (replaced) {
    return { lead: replaced.number != null ? `Replaced by #${replaced.number}` : "Replaced", step: null, tone: "cancelled" };
  }
  const word = statusWord(r.status);
  if ((r.status === "failed" || r.status === "dead_lettered") && r.failed_step) {
    const more = (r.failed_steps_count ?? 1) - 1;
    return { lead: `${word} at`, step: stepDisplayName(r.failed_step) + (more > 0 ? ` +${more}` : ""), tone: r.status };
  }
  if (r.status === "suspended" && r.waiting_for) {
    return { lead: `Waiting for ${waitingWord(r.waiting_for.kind)}`, step: null, tone: r.status };
  }
  return { lead: word, step: null, tone: r.status };
}

/** The phrase as one line of text, for a `title` and the tests. */
export function statusText(p: StatusPhrase): string {
  return p.step ? `${p.lead} ${p.step}` : p.lead;
}

// ── Replaced Runs ──────────────────────────────────────────────────────────

/** A row of the list once replaced Runs are placed. `child` rows sit under
 *  the Run that replaced them, indented; `replaced` is set on every Run that
 *  has a replacer, placed or not. */
export type Placed<T> = { row: T; child: boolean; replaced: Replaced | null };

type RowLike = { kind: string; at: number };
type RunLike = { id: string; run_number?: number | null; replaced_by?: string | null };

/** Puts each replaced Run directly under the Run that replaced it, following
 *  the chain to the newest (a Run replaced twice sits under the third). The
 *  replaced Runs under one Run keep their time order. A Run whose replacer is
 *  not in `rows` (not loaded, or hidden by a filter) keeps its place and has
 *  `replaced.number` null. `rows` is newest first; refused triggers never
 *  replace or get replaced. */
export function placeReplaced<T extends RowLike>(rows: readonly T[], run: (row: T) => RunLike | null): Placed<T>[] {
  const byId = new Map<string, T>();
  for (const row of rows) {
    const r = run(row);
    if (r) byId.set(r.id, row);
  }
  // The newest Run of the chain that is on the page, or null when the direct
  // replacer is not.
  const top = (row: T): T | null => {
    let cur = row;
    const seen = new Set<string>();
    for (;;) {
      const r = run(cur)!;
      seen.add(r.id);
      const next = r.replaced_by ? byId.get(r.replaced_by) : undefined;
      if (!next || seen.has(run(next)!.id)) return cur === row ? null : cur;
      cur = next;
    }
  };
  const under = new Map<T, T[]>();
  const placedUnder = new Set<T>();
  for (const row of rows) {
    const t = run(row) ? top(row) : null;
    if (!t) continue;
    placedUnder.add(row);
    const list = under.get(t) ?? [];
    list.push(row);
    under.set(t, list);
  }
  const replacedOf = (row: T): Replaced | null => {
    const by = run(row)?.replaced_by;
    if (!by) return null;
    const rep = byId.get(by);
    return { number: rep ? (run(rep)!.run_number ?? null) : null };
  };
  const out: Placed<T>[] = [];
  for (const row of rows) {
    if (placedUnder.has(row)) continue;
    out.push({ row, child: false, replaced: replacedOf(row) });
    for (const kid of under.get(row) ?? []) out.push({ row: kid, child: true, replaced: replacedOf(kid) });
  }
  return out;
}

// ── Day groups ─────────────────────────────────────────────────────────────

const WEEKDAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** The local calendar day of an instant, as a key that sorts and compares. */
function dayKey(ms: number): string {
  const d = new Date(ms);
  return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
}

/** A day header's name in local time: `Today`, `Yesterday`, else
 *  `Saturday 22 Nov` (with the year when it is not this year's). */
export function dayLabel(ms: number, now: number): string {
  const d = new Date(ms);
  if (dayKey(ms) === dayKey(now)) return "Today";
  const y = new Date(now);
  y.setDate(y.getDate() - 1);
  if (dayKey(ms) === dayKey(y.getTime())) return "Yesterday";
  const year = d.getFullYear() === new Date(now).getFullYear() ? "" : ` ${d.getFullYear()}`;
  return `${WEEKDAYS[d.getDay()]} ${d.getDate()} ${MONTHS[d.getMonth()]}${year}`;
}

/** A local clock time, `14:05`. */
export function clockTime(ms: number): string {
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

export type DayGroup<T> = { key: string; label: string; items: Placed<T>[]; count: number; failed: number };

/** Splits the placed list into local days, in list order. A replaced Run
 *  stays in the group of the Run it sits under. `failed` counts failed and
 *  dead-lettered Runs and refused triggers. */
export function dayGroups<T extends RowLike>(
  items: readonly Placed<T>[],
  status: (row: T) => string,
  now: number,
): DayGroup<T>[] {
  const out: DayGroup<T>[] = [];
  for (const item of items) {
    const key = dayKey(item.row.at);
    let g = out[out.length - 1];
    if (!g || (!item.child && g.key !== key)) {
      g = { key, label: dayLabel(item.row.at, now), items: [], count: 0, failed: 0 };
      out.push(g);
    }
    g.items.push(item);
    g.count += 1;
    const s = status(item.row);
    if (s === "failed" || s === "dead_lettered" || s === "refused") g.failed += 1;
  }
  return out;
}

/** A day header's summary: `9 runs · 3 failed or refused`, as its two parts. */
export function daySummary(g: { count: number; failed: number }): { runs: string; failed: string | null } {
  return {
    runs: `${g.count} ${g.count === 1 ? "run" : "runs"}`,
    failed: g.failed > 0 ? `${g.failed} failed or refused` : null,
  };
}
