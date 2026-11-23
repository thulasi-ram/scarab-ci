// One row of the Runs list, as words: the status mark, the headline, and the
// one phrase that says where a Run came from. Pure, so the list and its tests
// agree without a DOM.

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
  sha?: string | null;
  pr?: number | null;
  /** A pull request's base branch. */
  base?: string | null;
  actor?: string | null;
  /** A cron Run's title: the expression that fired. */
  title?: string | null;
};

/** One piece of the phrase. `sha` and `pr` are the forge links; `arrow`
 *  and `sep` carry their own spaces when drawn (` → `, ` · `). */
export type PhrasePart =
  | { t: "text"; text: string }
  | { t: "mono"; text: string }
  | { t: "muted"; text: string }
  | { t: "arrow" }
  | { t: "sep" }
  | { t: "sha"; text: string }
  | { t: "pr"; n: number };

/** Where a Run came from, as one phrase with the commit hash once:
 *  `main · bc99147 · a.kim`, `#212 feat/x → main · 3f9c2e1 · m.diaz`,
 *  `tag v0.7.2 · 1dcf093`, `schedule 0 2 * * * on main · f5a9de5`. */
export function originPhrase(o: Origin): PhrasePart[] {
  const ref = o.ref ?? null;
  const branch = ref && !ref.startsWith("refs/pull/") ? shortRef(ref) : null;
  const on = (lead: string): PhrasePart[] =>
    branch ? [{ t: "text", text: `${lead} on ` }, { t: "mono", text: branch }] : [{ t: "text", text: lead }];
  let what: PhrasePart[];
  switch (o.kind) {
    case null:
    case undefined:
    case "":
      what = [{ t: "muted", text: "no trigger recorded" }];
      break;
    case "push":
      what = branch ? [{ t: "mono", text: branch }] : [];
      break;
    case "pull_request": {
      const n = o.pr ?? pullNumber(ref);
      what = n != null ? [{ t: "pr", n }] : [];
      // The head branch only when the ref names one: a pull_request Run
      // records `refs/pull/<n>/head`, which says nothing `#n` does not.
      if (branch) {
        if (what.length > 0) what.push({ t: "text", text: " " });
        what.push({ t: "mono", text: branch });
      }
      if (o.base) what.push({ t: "arrow" }, { t: "mono", text: o.base });
      if (what.length === 0) what = [{ t: "text", text: "pull request" }];
      break;
    }
    case "tag":
    case "release":
      what = branch ? [{ t: "text", text: `${o.kind} ` }, { t: "mono", text: branch }] : [{ t: "text", text: o.kind }];
      break;
    case "cron":
      what = [{ t: "text", text: "schedule" }];
      if (o.title) what.push({ t: "text", text: " " }, { t: "mono", text: o.title });
      if (branch) what.push({ t: "text", text: " on " }, { t: "mono", text: branch });
      break;
    case "manual":
      what = on("manual");
      break;
    case "api":
      what = on("API");
      break;
    default:
      what = on(o.kind);
  }
  const out: PhrasePart[] = [...what];
  const add = (p: PhrasePart) => {
    if (out.length > 0) out.push({ t: "sep" });
    out.push(p);
  };
  if (o.sha) add({ t: "sha", text: o.sha.slice(0, 7) });
  if (o.actor) add({ t: "text", text: o.actor });
  return out;
}

/** The phrase as plain text, as it reads on screen. */
export function phraseText(parts: PhrasePart[]): string {
  return parts
    .map((p) => {
      switch (p.t) {
        case "arrow":
          return " → ";
        case "sep":
          return " · ";
        case "pr":
          return `#${p.n}`;
        default:
          return p.text;
      }
    })
    .join("");
}
