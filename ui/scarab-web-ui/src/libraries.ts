// Org-installed libraries (ADR-0076 §4-§6), as the Settings › Libraries panel
// reasons about them. Pure — no DOM, no fetch — so the panel and its tests
// agree on what a form sends and what a line says.
//
// Installing is two steps: fetch (the server reads the source, checks it,
// stores it and shows the diff) and install (by the content hash just
// reviewed, naming the hash it replaces). These helpers only let the form say
// what is wrong before the round trip; where they could disagree, the server
// wins and its text is shown.

const SEGMENT = /^[A-Za-z0-9_][A-Za-z0-9._-]{0,99}$/;
const COMMIT = /^([0-9a-f]{40}|[0-9a-f]{64})$/;

/** Why `source` is not `<host>/<owner>/<repo>//<path>@<commit>`, or `null`.
 *  An empty field is not an error yet. */
export function sourceError(source: string): string | null {
  const s = source.trim().replace(/^https:\/\//, "");
  if (s === "") return null;
  const at = s.lastIndexOf("@");
  if (at < 0) return "Add the commit: …@<40-character commit id>.";
  const commit = s.slice(at + 1);
  if (!COMMIT.test(commit)) {
    return "Pin the source to a full commit id (40 lowercase hex characters); a branch or tag can move after you review it.";
  }
  const location = s.slice(0, at);
  const split = location.indexOf("//");
  if (split < 0) return "Separate the repository from the file with //: github.com/acme/libs//deploy.yaml@<commit>.";
  const repo = location.slice(0, split).split("/");
  const path = location.slice(split + 2);
  if (repo.length !== 3 || !repo[0]!.includes(".") || !SEGMENT.test(repo[1]!) || !SEGMENT.test(repo[2]!)) {
    return "The repository is <host>/<owner>/<repo>, for example github.com/acme/scarab-libs.";
  }
  if (!/\.ya?ml$/.test(path) || path.split("/").some((p) => p === "" || p === "." || p === "..")) {
    return "The file is a repo-relative .yaml path.";
  }
  return null;
}

/** Why `segment` cannot be a publisher or name, or `null` (empty = default). */
export function segmentError(segment: string): string | null {
  if (segment === "" || SEGMENT.test(segment)) return null;
  return `"${segment}" is not a valid name: letters, digits, ".", "_" and "-", not starting with "." or "-".`;
}

/** The fetch form as the dialog holds it. */
export type FetchForm = {
  source: string;
  /** A ForgeConnection id; empty = fetch anonymously from a public forge. */
  connection: string;
  /** Empty = the source's owner. */
  publisher: string;
  /** Empty = the file name. */
  name: string;
};

/** What `POST …/libraries/fetch` carries — structurally the generated
 *  `FetchLibraryRequest`. */
export type FetchBody = {
  source: string;
  connection?: string;
  publisher?: string;
  name?: string;
};

/** The body for `form`, or the first reason it cannot be sent. */
export function toFetchBody(
  form: FetchForm,
): { body: FetchBody; error?: undefined } | { body?: undefined; error: string } {
  const source = form.source.trim();
  if (!source) return { error: "A source is required." };
  const problem = sourceError(source) ?? segmentError(form.publisher.trim()) ?? segmentError(form.name.trim());
  if (problem) return { error: problem };
  const body: FetchBody = { source };
  if (form.connection.trim()) body.connection = form.connection.trim();
  if (form.publisher.trim()) body.publisher = form.publisher.trim();
  if (form.name.trim()) body.name = form.name.trim();
  return { body };
}

/** The first 12 characters of a content hash, as refusals name a version. */
export function shortHash(hash: string): string {
  return hash.slice(0, 12);
}

/** One line of a fetched version's diff. */
export type DiffLine = { sign: " " | "+" | "-"; text: string };

/** The server's diff (`+`, `-` or a space before each line), as lines. */
export function diffLines(diff: string): DiffLine[] {
  return diff
    .split("\n")
    .filter((l, i, all) => !(l === "" && i === all.length - 1))
    .map((l) => {
      const sign = l[0] === "+" || l[0] === "-" ? l[0] : " ";
      return { sign, text: l.slice(1) };
    });
}

/** A yank reason, trimmed, or why it cannot be sent. */
export function yankReasonError(reason: string): string | null {
  const r = reason.trim();
  if (r === "") return "Give a reason: every refused Run shows it.";
  if (new TextEncoder().encode(r).length > 2000) return "Keep the reason under 2,000 bytes.";
  return null;
}

/** The update notice for an installation, as one line. */
export function updateLine(update: { release: string; security_fix: boolean } | null | undefined): string | null {
  if (!update) return null;
  return update.security_fix
    ? `${update.release} is available and fixes a security problem`
    : `${update.release} is available`;
}

/** What the org's defaults record says, as one line; `null` when there is
 *  nothing to tell. */
export function defaultsLine(
  d: { state: string; missing: { publisher: string; name: string; error: string }[] } | null | undefined,
): string | null {
  if (!d) return null;
  switch (d.state) {
    case "pending":
      return "Installing the first-party libraries…";
    case "missing":
      return `${d.missing.length === 1 ? "A first-party library is" : "First-party libraries are"} not installed: the source could not be read.`;
    default:
      return null;
  }
}

/** A missing default, named: the release manifest itself has no name. */
export function missingLabel(m: { publisher: string; name: string }): string {
  return m.name ? `lib:${m.publisher}/${m.name}` : "release manifest";
}

// --- Settings › Libraries table, yank dialog and upgrade review (ADR-0076) ---

/** A source without its commit: what trust with forge credentials is kept
 *  against (`scarab_policy::library::trust_key`). */
export function trustKey(source: string): string {
  const s = source.trim().replace(/^https:\/\//, "");
  const at = s.lastIndexOf("@");
  return at < 0 ? s : s.slice(0, at);
}

/** Why an installation may receive a long-lived forge credential (ADR-0076
 *  §9): the first-party clone library by default, or a source the org
 *  trusts. `null` = it may not. */
export function trustNote(
  l: { publisher: string; name: string; source: string; first_party: boolean },
  trusted: { source: string; trusted_by: string }[],
): string | null {
  if (l.first_party && l.publisher === "thulasi-ram" && l.name === "clone") {
    return "Trusted with forge credentials by default: the first-party clone library.";
  }
  const t = trusted.find((t) => t.source === trustKey(l.source));
  return t ? `Source trusted with forge credentials by ${t.trusted_by}.` : null;
}

const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** The impact line of the yank dialog. */
export function yankImpactLine(i: { runs: unknown[]; running_steps: number }): string {
  if (i.runs.length === 0) return "No Run in progress uses this version.";
  const runs = `${plural(i.runs.length, "Run")} in progress ${i.runs.length === 1 ? "uses" : "use"} this version`;
  return i.running_steps > 0 ? `${runs} · ${plural(i.running_steps, "Step")} running now` : runs;
}

/** The yank dialog's button: it says when it also cancels. */
export function yankButtonLabel(cancel: boolean, runs: number): string {
  return cancel && runs > 0 ? `Yank and cancel ${plural(runs, "Run")}` : "Yank version";
}

/** The run handle in an impact list: `web #418`, or the short id. */
export function impactRunLabel(r: { project: string; run_number?: number | null; id: string }): string {
  return r.run_number != null ? `${r.project} #${r.run_number}` : `${r.project} ${r.id.slice(0, 8)}`;
}

/** One row of a diff with line numbers on each side. */
export type DiffRow = { sign: " " | "+" | "-"; text: string; old?: number; new?: number };
/** A row, or a run of unchanged lines folded away. */
export type FoldedRow = DiffRow | { fold: number };

/** Number the server's diff lines on each side. */
export function numberDiff(lines: DiffLine[]): DiffRow[] {
  let o = 0;
  let n = 0;
  return lines.map((l) => {
    if (l.sign === "+") return { ...l, new: ++n };
    if (l.sign === "-") return { ...l, old: ++o };
    return { ...l, old: ++o, new: ++n };
  });
}

/** Unchanged lines more than `ctx` lines from a change, folded into one row
 *  each run. */
export function foldDiff(rows: DiffRow[], ctx = 2): FoldedRow[] {
  const near = rows.map(
    (r, k) => r.sign !== " " || rows.slice(Math.max(0, k - ctx), k + ctx + 1).some((x) => x.sign !== " "),
  );
  const out: FoldedRow[] = [];
  let skipped = 0;
  rows.forEach((r, k) => {
    if (near[k]) {
      if (skipped) out.push({ fold: skipped });
      skipped = 0;
      out.push(r);
    } else skipped++;
  });
  if (skipped) out.push({ fold: skipped });
  return out;
}

/** A side-by-side cell: a line, a gap opposite an added or removed line, or
 *  a fold. */
export type SideCell = DiffRow | { blank: true } | { fold: number };

/** The folded diff as two columns: removals paired with the additions that
 *  follow them. */
export function sideBySide(rows: FoldedRow[]): { left: SideCell[]; right: SideCell[] } {
  const left: SideCell[] = [];
  const right: SideCell[] = [];
  for (let k = 0; k < rows.length; k++) {
    const r = rows[k]!;
    if ("fold" in r || r.sign === " ") {
      left.push(r);
      right.push(r);
      continue;
    }
    const dels: DiffRow[] = [];
    const adds: DiffRow[] = [];
    while (k < rows.length && !("fold" in rows[k]!) && (rows[k] as DiffRow).sign === "-") dels.push(rows[k++] as DiffRow);
    while (k < rows.length && !("fold" in rows[k]!) && (rows[k] as DiffRow).sign === "+") adds.push(rows[k++] as DiffRow);
    k--;
    for (let x = 0; x < Math.max(dels.length, adds.length); x++) {
      left.push(dels[x] ?? { blank: true });
      right.push(adds[x] ?? { blank: true });
    }
  }
  return { left, right };
}

/** The two files a diff joins: the installed one and the fetched one. */
export function diffSides(lines: DiffLine[]): { old: string; new: string } {
  return {
    old: lines.filter((l) => l.sign !== "+").map((l) => l.text).join("\n"),
    new: lines.filter((l) => l.sign !== "-").map((l) => l.text).join("\n"),
  };
}

/** The input names a library file declares under `interface.inputs`, read
 *  line by line (`- { name: x, … }` or `- name: x`). A best effort for the
 *  review page; the server's compile is the check. */
export function declaredInputs(content: string): string[] {
  const lines = content.split("\n");
  const start = lines.findIndex((l) => /^\s*inputs:\s*$/.test(l));
  if (start < 0) return [];
  const indent = lines[start]!.search(/\S/);
  const names: string[] = [];
  for (const l of lines.slice(start + 1)) {
    if (l.trim() === "") continue;
    if (l.search(/\S/) <= indent) break;
    const m = l.match(/^\s*-\s*(?:\{\s*)?name:\s*["']?([A-Za-z0-9_-]+)/);
    if (m) names.push(m[1]!);
  }
  return names;
}

/** One line of the review page's check list. */
export type ReviewCheck = { tone: "ok" | "info" | "warn"; text: string };

/** What the review page checks in a fetched version, from its content and
 *  the org's installations. The fetch already compiled it as a closed
 *  library; these lines say what that means for this file. */
export function reviewChecks(
  content: string,
  firstParty: boolean,
  installed: { publisher: string; name: string; content_hash: string }[],
): ReviewCheck[] {
  const out: ReviewCheck[] = [
    { tone: "ok", text: "Compiles as a library that invokes only installed lib: and plugin: targets" },
  ];
  const invoked = [...new Set([...content.matchAll(/invoke:\s*["']?lib:([A-Za-z0-9._-]+\/[A-Za-z0-9._-]+)/g)].map((m) => m[1]!))];
  for (const id of invoked) {
    const i = installed.find((x) => `${x.publisher}/${x.name}` === id);
    out.push(
      i
        ? { tone: "ok", text: `Invokes lib:${id}, installed here at ${shortHash(i.content_hash)}` }
        : { tone: "warn", text: `Invokes lib:${id}, which is not installed here` },
    );
  }
  const images = [...content.matchAll(/^\s*(?:-\s*)?image:\s*["']?([^\s"']+)/gm)].map((m) => m[1]!);
  const unpinned = images.filter((i) => !i.includes("@sha256:"));
  if (images.length > 0) {
    out.push(
      unpinned.length === 0
        ? { tone: "ok", text: "Every image is pinned by digest" }
        : { tone: "warn", text: `Not pinned by digest: ${unpinned.join(", ")}` },
    );
  }
  out.push(
    /\bforge_token\b/.test(content)
      ? { tone: "warn", text: "A Step asks for forge_token: it receives a forge credential only if this source is trusted" }
      : { tone: "info", text: "No Step asks for forge_token, so trust with forge credentials does not apply" },
  );
  out.push(
    firstParty
      ? { tone: "ok", text: "First-party: the hash is in Scarab's release manifest" }
      : { tone: "info", text: "Not first-party: the hash is not in a Scarab release manifest" },
  );
  return out;
}

/** What an upgrade does to the library's inputs, as one line. */
export function inputsChange(oldContent: string, newContent: string): string {
  const before = declaredInputs(oldContent);
  const after = declaredInputs(newContent);
  const added = after.filter((n) => !before.includes(n));
  const removed = before.filter((n) => !after.includes(n));
  if (added.length === 0 && removed.length === 0) return "no input added or removed";
  const parts = [];
  if (added.length) parts.push(`adds ${added.join(", ")}`);
  parts.push(removed.length ? `removes ${removed.join(", ")}` : "no input removed");
  return parts.join("; ");
}

/** The review page's URL for a library: a fresh install, or an upgrade
 *  from `source`. */
export function reviewHref(p: { source?: string; publisher?: string; name?: string; connection?: string }): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(p)) if (v) q.set(k, v);
  const s = q.toString();
  return `/settings/libraries/review${s ? `?${s}` : ""}`;
}
