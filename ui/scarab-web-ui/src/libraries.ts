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
