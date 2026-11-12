// Org-installed libraries, as the Settings › Libraries panel
// reasons about them. Pure — no DOM, no fetch — so the panel and its tests
// agree on what a form sends and what a line says.
//
// Installing is two steps: fetch (the server reads the source, checks it,
// stores it and shows the diff) and install (by the content hash just
// reviewed, naming the hash it replaces). These helpers only let the form say
// what is wrong before the round trip; where they could disagree, the server
// wins and its text is shown.
import { libraryReviewHref, orgHref } from "./settings-nav";

const SEGMENT = /^[A-Za-z0-9_][A-Za-z0-9._-]{0,99}$/;
const COMMIT = /^([0-9a-f]{40}|[0-9a-f]{64})$/;

/** Why `location` is not `<host>/<owner>/<repo>//<path>` with a path
 *  `file` accepts, or `null`. */
function locationError(location: string, file: { ext: RegExp; example: string; hint: string }): string | null {
  const split = location.indexOf("//");
  if (split < 0) return `Separate the repository from the file with //: github.com/acme/${file.example}.`;
  const repo = location.slice(0, split).split("/");
  const path = location.slice(split + 2);
  if (repo.length !== 3 || !repo[0]!.includes(".") || !SEGMENT.test(repo[1]!) || !SEGMENT.test(repo[2]!)) {
    return "The repository is <host>/<owner>/<repo>, for example github.com/acme/scarab-libs.";
  }
  if (!file.ext.test(path) || path.split("/").some((p) => p === "" || p === "." || p === "..")) {
    return file.hint;
  }
  return null;
}

/** Why `source` is not `<host>/<owner>/<repo>//<path>@<commit>` with a
 *  path `file` accepts, or `null`. An empty field is not an error yet. The
 *  plugin manifest form reads sources this way. */
export function pinnedSourceError(
  source: string,
  file: { ext: RegExp; example: string; hint: string },
): string | null {
  const s = source.trim().replace(/^https:\/\//, "");
  if (s === "") return null;
  const at = s.lastIndexOf("@");
  if (at < 0) return "Add the commit: …@<40-character commit id>.";
  const commit = s.slice(at + 1);
  if (!COMMIT.test(commit)) {
    return "Pin the source to a full commit id (40 lowercase hex characters); a branch or tag can move after you review it.";
  }
  return locationError(s.slice(0, at), { ...file, example: `${file.example}@<commit>` });
}

const LIBRARY_FILE = {
  ext: /\.ya?ml$/,
  example: "libs//deploy.yaml",
  hint: "The file is a repo-relative .yaml path.",
};

/** A library source split at its `@`: the file, and the branch, tag or
 *  commit after the `@` (empty when there is none). A file path holds no
 *  `@`, so the first one starts the ref. */
export function splitSource(source: string): { source: string; ref: string } {
  const s = source.trim().replace(/^https:\/\//, "");
  const at = s.indexOf("@");
  return at < 0 ? { source: s, ref: "" } : { source: s.slice(0, at), ref: s.slice(at + 1).trim() };
}

/** Why `source` is not a library's file, `<host>/<owner>/<repo>//<path>`,
 *  or `null`. An empty field is not an error yet. A `…@<ref>` is read as
 *  the file and its ref. */
export function sourceError(source: string): string | null {
  const s = splitSource(source).source;
  if (s === "") return null;
  return locationError(s, LIBRARY_FILE);
}

/** Why `ref` cannot name a branch, tag or commit, or `null`. An empty field
 *  is not an error yet. */
export function refError(ref: string): string | null {
  const r = ref.trim();
  if (r === "") return null;
  if (r.length > 255) return "A ref is at most 255 characters.";
  if (!/^[\x21-\x7e]+$/.test(r)) return "A ref is printable ASCII with no spaces.";
  if (r === "." || r === "..") return `"${r}" is not a ref.`;
  return null;
}

/** Whether `ref` is a full commit id, which needs no lookup. */
export function isFullCommit(ref: string): boolean {
  return COMMIT.test(ref.trim());
}

/** The library name rule, as the server states it. */
export const LIBRARY_NAME_RULE =
  "a library name is parts of letters, digits, `.`, `_` and `-`, joined by `::` or `/`, each part starting with a letter, digit or `_`, at most 255 bytes in all";

const NAME_PART = /^[A-Za-z0-9_][A-Za-z0-9._-]*$/;

/** Whether `name` is a library name: 1 to 255 ASCII bytes, split into parts
 *  on `::` and `/`, every part starting with a letter, digit or `_` and
 *  holding only letters, digits, `.`, `_` and `-`. The server's rule; it
 *  checks again. */
export function isLibraryName(name: string): boolean {
  if (name.length === 0 || name.length > 255 || !/^[\x00-\x7f]*$/.test(name)) return false;
  return name
    .split("/")
    .flatMap((p) => p.split("::"))
    .every((part) => NAME_PART.test(part));
}

/** Why `name` is not a library name, or `null`. An empty field is not an
 *  error: the fetch then uses the default name. */
export function libraryNameError(name: string): string | null {
  if (name === "" || isLibraryName(name)) return null;
  return `"${name}" is not a library name: ${LIBRARY_NAME_RULE}.`;
}

/** The name a fetch gives a library when none is typed:
 *  `<owner>::<repo>::<file name without .yaml or .yml>`, read from the
 *  source field. `null` when the field does not name a file yet. */
export function defaultLibraryName(source: string): string | null {
  const s = splitSource(source).source;
  const split = s.indexOf("//");
  if (split < 0) return null;
  const repo = s.slice(0, split).split("/");
  const file = s.slice(split + 2).split("/").pop() ?? "";
  if (repo.length !== 3 || !repo[1] || !repo[2] || !file) return null;
  const stem = file.endsWith(".yaml") ? file.slice(0, -5) : file.endsWith(".yml") ? file.slice(0, -4) : file;
  return stem ? `${repo[1]}::${repo[2]}::${stem}` : null;
}

/** The fetch form as the dialog holds it. */
export type FetchForm = {
  /** `<host>/<owner>/<repo>//<path>`; a `…@<ref>` here stands for `ref`. */
  source: string;
  /** A branch, tag or commit. Empty = the one after `@` in `source`. */
  ref: string;
  /** A ForgeConnection id; empty = fetch anonymously from a public forge. */
  connection: string;
  /** Empty = `<owner>::<repo>::<file name>` (`defaultLibraryName`). */
  name: string;
};

/** What `POST …/libraries/fetch` carries — structurally the generated
 *  `FetchLibraryRequest`. A full commit goes in `source` after `@`; a
 *  branch or tag goes in `ref`, and the server reads it at the commit it
 *  points to now. */
export type FetchBody = {
  source: string;
  ref?: string;
  connection?: string;
  name?: string;
};

/** The body for `form`, or the first reason it cannot be sent. */
export function toFetchBody(
  form: FetchForm,
): { body: FetchBody; error?: undefined } | { body?: undefined; error: string } {
  const split = splitSource(form.source);
  const location = split.source;
  const ref = form.ref.trim() || split.ref;
  if (!location) return { error: "A source is required." };
  if (!ref) return { error: "A branch, tag or commit is required." };
  const problem = sourceError(location) ?? refError(ref) ?? libraryNameError(form.name.trim());
  if (problem) return { error: problem };
  const body: FetchBody = isFullCommit(ref) ? { source: `${location}@${ref}` } : { source: location, ref };
  if (form.connection.trim()) body.connection = form.connection.trim();
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

// --- Settings › Libraries table, yank dialog and upgrade review ---

/** A source without its commit: what trust with forge credentials is kept
 *  against (`scarab_policy::library::trust_key`). */
export function trustKey(source: string): string {
  const s = source.trim().replace(/^https:\/\//, "");
  const at = s.lastIndexOf("@");
  return at < 0 ? s : s.slice(0, at);
}

/** Why an installation may receive a long-lived forge credential: its
 *  source is one the org trusts. `null` = it may not. No library is trusted
 *  by default, whoever publishes it. */
export function trustNote(
  l: { source: string },
  trusted: { source: string; trusted_by: string; grant?: string }[],
): string | null {
  const t = trusted.find((t) => t.source === trustKey(l.source) && isForgeTrust(t));
  return t ? `Source trusted with forge credentials by ${t.trusted_by}.` : null;
}

/** What a trust grant gives, as a sentence reads it. */
export function grantLabel(grant: string | undefined): string {
  if (grant === "build_sandbox") return "the build sandbox";
  if (grant === "registry_credential") return "the registry credential";
  return "forge credentials";
}

/** What stopping trust with `grant` does, as the confirm dialog says it. */
export function untrustEffect(grant: string | undefined): string {
  if (grant === "registry_credential") {
    return "From now on a Step from this source with registry_auth gets no registry credential, in new Runs and at each launch of a Run created before.";
  }
  return `Runs created from now on refuse a Step from this source that needs ${grantLabel(grant)}, and so does each launch of a Run created before.`;
}

/** Whether a trust record grants forge credentials. A record with no grant
 *  is one: that was the only grant before the build sandbox. */
export function isForgeTrust(t: { grant?: string }): boolean {
  return (t.grant ?? "forge_credentials") === "forge_credentials";
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
  installed: { name: string; content_hash: string }[],
): ReviewCheck[] {
  const out: ReviewCheck[] = [
    { tone: "ok", text: "Compiles as a library that invokes only installed lib: and plugin: targets" },
  ];
  const invoked = [...new Set([...content.matchAll(/invoke:\s*["']?lib:([A-Za-z0-9._:/-]+)/g)].map((m) => m[1]!))];
  for (const id of invoked) {
    const i = installed.find((x) => x.name === id);
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

/** The review page's URL for a library in `org`: a fresh install, or an
 *  upgrade from `source`. */
export function reviewHref(
  org: string,
  p: { source?: string; ref?: string; name?: string; connection?: string },
): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(p)) if (v) q.set(k, v);
  const s = q.toString();
  return `${libraryReviewHref(org)}${s ? `?${s}` : ""}`;
}

// --- Settings › Libraries list and each library's manage page ---

/** How pipelines name an installation: `lib:<name>`. */
export function libraryId(l: { name: string }): string {
  return `lib:${l.name}`;
}

/** The manage page of one installation in `org`, by its id. */
export function libraryManageHref(org: string, l: { id: string }): string {
  return `${orgHref(org, "libraries")}/${encodeURIComponent(l.id)}/manage`;
}

/** A small state tag on a row or a heading. */
export type ChipTone = "ok" | "motion" | "danger" | "muted";
export type Chip = { label: string; tone: ChipTone; title: string };

/** The state tags of one installation, most urgent first: yanked, then
 *  whether its source is trusted with forge credentials. */
export function libraryChips(
  l: { source: string; yanked?: { reason: string; yanked_by: string } | null },
  trusted: { source: string; trusted_by: string; grant?: string }[],
): Chip[] {
  const out: Chip[] = [];
  if (l.yanked) {
    out.push({ label: "yanked", tone: "danger", title: `Yanked by ${l.yanked.yanked_by}: ${l.yanked.reason}` });
  }
  const trust = trustNote(l, trusted);
  out.push(
    trust
      ? { label: "trusted source", tone: "ok", title: trust }
      : {
          label: "untrusted source",
          tone: "muted",
          title: "No Step from this source receives a long-lived forge token until the org trusts the source.",
        },
  );
  const sandbox = trusted.find((t) => t.source === trustKey(l.source) && t.grant === "build_sandbox");
  if (sandbox) {
    out.push({
      label: "build sandbox",
      tone: "ok",
      title: `Source trusted with the build sandbox by ${sandbox.trusted_by}.`,
    });
  }
  const registry = trusted.find((t) => t.source === trustKey(l.source) && t.grant === "registry_credential");
  if (registry) {
    out.push({
      label: "registry credential",
      tone: "ok",
      title: `Source trusted with the registry credential by ${registry.trusted_by}.`,
    });
  }
  return out;
}

/** What a yank says about its version: the library it was fetched for and
 *  the pinned source. */
type YankOf = { content_hash: string; name: string; yanked_at: number };

/** Every yank of `lib`'s versions, newest first: the yanks of its name, and
 *  the installed version's whatever it was first fetched for. `installed` marks the installed version's. The manage page
 *  lists them. */
export function yanksOfLibrary<Y extends YankOf>(
  yanks: readonly Y[],
  lib: { name: string; content_hash: string },
): (Y & { installed: boolean })[] {
  return yanks
    .filter((y) => y.content_hash === lib.content_hash || y.name === lib.name)
    .map((y) => ({ ...y, installed: y.content_hash === lib.content_hash }))
    .sort((a, b) => b.yanked_at - a.yanked_at);
}

/** Yanks of libraries with no installation at all: the list page keeps
 *  them, since no manage page does. */
export function removedLibraryYanks<Y extends YankOf>(
  yanks: readonly Y[],
  installed: readonly { name: string; content_hash: string }[],
): Y[] {
  return yanks.filter((y) => !installed.some((i) => yanksOfLibrary([y], i).length > 0));
}

/** A version as a person reads it: its source with the commit cut to 7
 *  characters, `github.com/acme/libs//deploy.yaml@8c2f4e1`. */
export function versionLabel(source: string): string {
  const at = source.lastIndexOf("@");
  return at < 0 ? source : `${source.slice(0, at)}@${source.slice(at + 1, at + 8)}`;
}

/** Trusted sources no installation comes from now: the list page keeps
 *  them, since no manage page does. */
export function strayTrust<T extends { source: string }>(
  trusted: readonly T[],
  installed: readonly { source: string }[],
): T[] {
  return trusted.filter((t) => !installed.some((i) => trustKey(i.source) === t.source));
}

/** Each trusted source with the installations that come from it now, for
 *  the read-only list under Library policies. An empty `libraries`: no
 *  installation comes from the source. */
export function trustedSourceRows<T extends { source: string }, I extends { source: string }>(
  trusted: readonly T[],
  installed: readonly I[],
): (T & { libraries: I[] })[] {
  return trusted.map((t) => ({ ...t, libraries: installed.filter((i) => trustKey(i.source) === t.source) }));
}
