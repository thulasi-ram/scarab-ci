// Pure helpers behind the Run page's Manifest tab: the Pipeline file split into
// lines and coloured tokens, the top-level steps found by a line scan (no YAML
// library), each step's result aggregated over the Run's Steps it became, and
// the library boxes after its `invoke:` line. No DOM, so the vitest suite
// covers them (manifest.test.ts). The file is repo content: the tokens are
// plain text, and the page renders them as text nodes.
import { parseMatrixId, statusWord } from "./run-page";

// ── Lines and tokens ────────────────────────────────────────────────────────

export type TokenClass = "key" | "punct" | "comment" | "string" | "expr" | "ref";
export type Token = { text: string; cls?: TokenClass };

/** The file's lines. A final newline does not add an empty line; `\r\n` reads
 * as `\n`. */
export function fileLines(source: string): string[] {
  if (source === "") return [];
  const lines = source.replace(/\r\n?/g, "\n").split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

const indentOf = (line: string): number => line.length - line.trimStart().length;
const isBlank = (line: string): boolean => line.trim() === "";
const isCommentOnly = (line: string): boolean => /^\s*#/.test(line);

/** Characters after which a quote opens a quoted scalar: anywhere else (`it's`)
 * it is a plain character. */
const QUOTE_AFTER = /[\s[{,:]/;

/** Where the comment starts in a line outside a block scalar, or -1. A `#`
 * starts a comment at the line start or after whitespace, outside quotes and
 * outside a `${{ … }}` expression. Also returns the quoted spans it passed. */
function scanLine(line: string): { comment: number; quoted: [number, number][] } {
  const quoted: [number, number][] = [];
  let quote: '"' | "'" | null = null;
  let openAt = 0;
  let expr = false;
  for (let i = 0; i < line.length; i++) {
    const c = line[i]!;
    if (quote) {
      if (quote === '"' && c === "\\") {
        i++;
        continue;
      }
      if (c === quote) {
        quote = null;
        quoted.push([openAt, i + 1]);
      }
      continue;
    }
    if (expr) {
      if (c === "}" && line[i + 1] === "}") {
        expr = false;
        i++;
      }
      continue;
    }
    if (c === "$" && line.startsWith("${{", i)) {
      expr = true;
      i += 2;
      continue;
    }
    if ((c === '"' || c === "'") && (i === 0 || QUOTE_AFTER.test(line[i - 1]!))) {
      quote = c;
      openAt = i;
      continue;
    }
    if (c === "#" && (i === 0 || /\s/.test(line[i - 1]!))) return { comment: i, quoted };
  }
  if (quote) quoted.push([openAt, line.length]);
  return { comment: -1, quoted };
}

/** Group per-character classes into tokens. */
function toTokens(line: string, cls: (TokenClass | undefined)[]): Token[] {
  const out: Token[] = [];
  let start = 0;
  for (let i = 1; i <= line.length; i++) {
    if (i === line.length || cls[i] !== cls[start]) {
      const t: Token = { text: line.slice(start, i) };
      if (cls[start]) t.cls = cls[start];
      out.push(t);
      start = i;
    }
  }
  return out;
}

function markExprs(line: string, cls: (TokenClass | undefined)[], from: number, to: number) {
  const re = /\$\{\{[\s\S]*?\}\}/g;
  const part = line.slice(from, to);
  for (let m = re.exec(part); m; m = re.exec(part)) {
    for (let i = from + m.index; i < from + m.index + m[0].length; i++) cls[i] = "expr";
  }
}

/** The tokens of one line outside a block scalar. */
export function tokenizeLine(line: string): Token[] {
  if (line === "") return [];
  const cls: (TokenClass | undefined)[] = new Array(line.length).fill(undefined);
  const { comment, quoted } = scanLine(line);
  const end = comment < 0 ? line.length : comment;
  for (let i = end; i < line.length; i++) cls[i] = "comment";
  const code = line.slice(0, end);
  // The leading list dashes and the first key: `  - id:`.
  const lead = /^(\s*)((?:-\s+)*)([\w.-]+)?(:)?(?=\s|$)/.exec(code);
  if (lead) {
    let at = lead[1]!.length;
    for (const ch of lead[2]!) {
      if (ch === "-") cls[at] = "punct";
      at++;
    }
    if (lead[3] && lead[4]) {
      for (let i = at; i < at + lead[3].length; i++) cls[i] = "key";
      cls[at + lead[3].length] = "punct";
    }
  }
  // Keys inside a flow mapping: `{ id: g, invoke: … }`.
  const inQuote = (i: number) => quoted.some(([a, b]) => i >= a && i < b);
  const flowKey = /([{,]\s*)([\w.-]+)(:)(?=\s)/g;
  for (let m = flowKey.exec(code); m; m = flowKey.exec(code)) {
    const k = m.index + m[1]!.length;
    if (inQuote(k)) continue;
    for (let i = k; i < k + m[2]!.length; i++) cls[i] = "key";
    cls[k + m[2]!.length] = "punct";
  }
  // References to what a step invokes: `lib:…`, `file:…`, `plugin:…`.
  const ref = /(^|[\s[{,])((?:lib|file|plugin):[^\s,\]}'"#]+)/g;
  for (let m = ref.exec(code); m; m = ref.exec(code)) {
    const k = m.index + m[1]!.length;
    if (inQuote(k)) continue;
    for (let i = k; i < k + m[2]!.length; i++) cls[i] = "ref";
  }
  for (const [a, b] of quoted) for (let i = a; i < Math.min(b, end); i++) cls[i] = "string";
  markExprs(line, cls, 0, end);
  return toTokens(line, cls);
}

/** The column a block scalar's content must go past, when `line` opens one
 * (`run: |`, `- |`, `script: >-`), else null. */
function blockScalarOwner(line: string): number | null {
  const { comment } = scanLine(line);
  const code = (comment < 0 ? line : line.slice(0, comment)).trimEnd();
  const item = /^(\s*)-\s+(.*)$/.exec(code);
  if (item) {
    // `- |` is owned by the dash; `- run: |` by the key after it.
    if (/^[|>][-+1-9]{0,2}$/.test(item[2]!)) return item[1]!.length;
    if (/^[\w.-]+:\s+[|>][-+1-9]{0,2}$/.test(item[2]!)) return code.length - item[2]!.length;
    return null;
  }
  return /^\s*[\w.-]+:\s+[|>][-+1-9]{0,2}$/.test(code) ? indentOf(code) : null;
}

/** Every line's tokens. The content of a block scalar (`|` or `>`) is text: a
 * `#` or a `key:` in it is not YAML, so only its `${{ … }}` expressions are
 * coloured. */
export function tokenizeFile(lines: string[]): Token[][] {
  const out: Token[][] = [];
  let owner: number | null = null;
  for (const line of lines) {
    if (owner !== null && (isBlank(line) || indentOf(line) > owner)) {
      const cls: (TokenClass | undefined)[] = new Array(line.length).fill(undefined);
      markExprs(line, cls, 0, line.length);
      out.push(line === "" ? [] : toTokens(line, cls));
      continue;
    }
    owner = blockScalarOwner(line);
    out.push(tokenizeLine(line));
  }
  return out;
}

// ── Top-level steps ─────────────────────────────────────────────────────────

/** One item of the file's top-level `steps:` list. Lines are 0-based. */
export type FileStep = {
  id: string;
  /** The line its `id:` is written on. */
  line: number;
  /** Its first line (the `-`) and its last line that is not blank or only a
   * comment. */
  start: number;
  end: number;
  /** The line of its `invoke:`, and what it invokes (`lib:…`). */
  invokeLine: number | null;
  invoke: string | null;
};

/** A scalar value as written: quotes and a trailing comment removed. */
function scalar(raw: string): string {
  const v = raw.trim();
  const q = /^"((?:[^"\\]|\\.)*)"|^'((?:[^']|'')*)'/.exec(v);
  if (q) return q[1] ?? q[2]!.replace(/''/g, "'");
  return v.replace(/\s+#.*$/, "").trim();
}

/** A value from a flow mapping line: `{ id: g, invoke: plugin:a/b }`. */
function flowValue(text: string, key: string): string | null {
  const m = new RegExp(`[{,]\\s*${key}:\\s*("(?:[^"\\\\]|\\\\.)*"|'(?:[^']|'')*'|[^,}]+)`).exec(text);
  return m ? scalar(m[1]!) : null;
}

/**
 * The items of the top-level `steps:` list, found by a line scan. The list's
 * item indent is the indent of the first `-` after `steps:`, so 2 spaces, 4
 * or none all read. An item's `id:` is on its `-` line or on a later line at
 * the item's key indent; an item written as a flow mapping (`- { id: x, … }`)
 * reads too. An item without an id is skipped. A `steps:` written inline
 * (`steps: [ … ]`) gives no items.
 */
export function fileSteps(lines: string[]): FileStep[] {
  const at = lines.findIndex((l) => /^steps:\s*(#.*)?$/.test(l));
  if (at < 0) return [];
  const out: FileStep[] = [];
  let itemIndent = -1;
  let cur: { start: number; keyIndent: number; flow: boolean; last: number; id: string | null; line: number; invokeLine: number | null; invoke: string | null } | null = null;
  let owner: number | null = null; // block scalar inside the current item
  const close = () => {
    if (cur?.id) {
      out.push({ id: cur.id, line: cur.line, start: cur.start, end: cur.last, invokeLine: cur.invokeLine, invoke: cur.invoke });
    }
    cur = null;
  };
  for (let i = at + 1; i < lines.length; i++) {
    const l = lines[i]!;
    if (isBlank(l)) continue;
    if (owner !== null && indentOf(l) > owner) {
      cur!.last = i;
      continue;
    }
    owner = null;
    if (isCommentOnly(l)) continue;
    const ind = indentOf(l);
    const dash = /^(\s*)-(\s+|$)(.*)$/.exec(l);
    if (itemIndent < 0) {
      if (!dash) break;
      itemIndent = ind;
    }
    if (ind < itemIndent || (ind === itemIndent && !dash)) break;
    if (ind === itemIndent) {
      close();
      const rest = dash![3]!;
      const flow = rest.startsWith("{");
      cur = {
        start: i,
        // A bare `-` puts its keys on the next lines: their indent is read there.
        keyIndent: rest === "" ? -1 : ind + 1 + dash![2]!.length,
        flow,
        last: i,
        id: null,
        line: i,
        invokeLine: null,
        invoke: null,
      };
      if (flow) {
        cur.id = flowValue(rest, "id");
        const inv = flowValue(rest, "invoke");
        if (inv) {
          cur.invokeLine = i;
          cur.invoke = inv;
        }
        continue;
      }
    }
    if (!cur) continue;
    const c: NonNullable<typeof cur> = cur;
    c.last = i;
    if (c.keyIndent < 0 && i !== c.start) c.keyIndent = ind;
    // The item's own keys: on the `-` line, or at its key indent.
    const field =
      i === c.start
        ? /^\s*-\s+([\w.-]+):(?:\s+(.*))?$/.exec(l)
        : ind === c.keyIndent
          ? /^\s*([\w.-]+):(?:\s+(.*))?$/.exec(l)
          : null;
    if (field) {
      const value = field[2] ?? "";
      if (field[1] === "id" && c.id === null) {
        c.id = scalar(value) || null;
        c.line = i;
      } else if (field[1] === "invoke" && c.invokeLine === null) {
        c.invokeLine = i;
        c.invoke = scalar(value) || null;
      }
    }
    owner = blockScalarOwner(l);
  }
  close();
  return out;
}

// ── Results by top-level step ───────────────────────────────────────────────

export type RunStepLike = { id: string; status: string };

/** The top-level step a Run Step comes from: its id up to the first `/` (a
 * Step inlined from an `invoke:`) or `[` (a matrix cell). */
export function stepRoot(id: string): string {
  const i = id.search(/[/[]/);
  return i < 0 ? id : id.slice(0, i);
}

/** The Run's Steps that top-level step `id` became, in the Run's order. */
export function stepsOf<S extends RunStepLike>(id: string, steps: S[]): S[] {
  return steps.filter((s) => stepRoot(s.id) === id);
}

/** The cells of step `id`'s own matrix (`test[os=linux]`), not those of a
 * Step inlined under it. */
export function matrixLegs<S extends RunStepLike>(id: string, steps: S[]): S[] {
  return steps.filter((s) => parseMatrixId(s.id)?.base === id);
}

export type MarkKind = "passed" | "failed" | "mixed" | "skipped" | "notrun" | "running" | "cancelled";

/** One Run Step's status as a mark. */
export function markOf(status: string): MarkKind {
  switch (status) {
    case "succeeded":
      return "passed";
    case "failed":
    case "dead_lettered":
      return "failed";
    case "running":
      return "running";
    case "skipped":
      return "skipped";
    case "cancelled":
    case "superseded":
      return "cancelled";
  }
  return "notrun";
}

/** What one top-level step did, over the Run Steps it became: its mark, the
 * mark in words for the hover title, and the Step a click opens (the first
 * that failed, else the first running, else the first). */
export type StepMark = { kind: MarkKind; words: string; open: string | null };

export function stepMark(id: string, steps: RunStepLike[]): StepMark {
  const own = stepsOf(id, steps);
  if (own.length === 0) return { kind: "notrun", words: "not in this Run", open: null };
  const kinds = own.map((s) => markOf(s.status));
  const count = (k: MarkKind) => kinds.filter((x) => x === k).length;
  const pick = (k: MarkKind) => own[kinds.indexOf(k)]?.id;
  const open = pick("failed") ?? pick("running") ?? own[0]!.id;
  if (own.length === 1) {
    const w = kinds[0] === "notrun" ? "not run yet" : statusWord(own[0]!.status).toLowerCase();
    return { kind: kinds[0]!, words: w, open };
  }
  const n = own.length;
  const noun = matrixLegs(id, own).length === n ? "legs" : "steps";
  const failed = count("failed");
  const passed = count("passed");
  const running = count("running");
  if (failed > 0) {
    return { kind: passed > 0 ? "mixed" : "failed", words: `${failed} of ${n} ${noun} failed`, open };
  }
  if (running > 0) return { kind: "running", words: `${running} of ${n} ${noun} running`, open };
  if (count("notrun") > 0) {
    return passed > 0
      ? { kind: "running", words: `${passed} of ${n} ${noun} passed, the rest not run yet`, open }
      : { kind: "notrun", words: `${n} ${noun}, none run yet`, open };
  }
  if (count("skipped") === n) return { kind: "skipped", words: `all ${n} ${noun} skipped`, open };
  if (count("cancelled") > 0) return { kind: "cancelled", words: `${count("cancelled")} of ${n} ${noun} cancelled`, open };
  if (passed === n) return { kind: "passed", words: `all ${n} ${noun} passed`, open };
  return { kind: "passed", words: `${passed} of ${n} ${noun} passed, ${n - passed} skipped`, open };
}

/** The Run Steps no line of the file names: a gate an Environment added, or a
 * step whose top-level item the scan did not find. */
export function unmappedSteps<S extends RunStepLike>(file: FileStep[], steps: S[]): S[] {
  const ids = new Set(file.map((f) => f.id));
  return steps.filter((s) => !ids.has(stepRoot(s.id)));
}

/** The first top-level step that failed, in file order, or null. */
export function firstFailure(file: FileStep[], steps: RunStepLike[]): FileStep | null {
  return file.find((f) => ["failed", "mixed"].includes(stepMark(f.id, steps).kind)) ?? null;
}

// ── What an invoke became ───────────────────────────────────────────────────

export type InvokedLike = { kind: string; name: string; content_hash: string; steps: string[] };
export type YankLike = { name: string; content_hash: string };

export type InvokeBox<I extends InvokedLike> = {
  entry: I;
  /** The Run Steps it became that came from this top-level step. */
  steps: string[];
  /** The `invoke:` line names this entry; a nested library is not direct. */
  direct: boolean;
  /** This library version was yanked in the org since the Run. */
  yanked: boolean;
};

/** How an `invoke:` value names an invoked entry. */
export function invokeTarget(e: { kind: string; name: string }): string {
  return `${e.kind === "library" ? "lib" : e.kind}:${e.name}`;
}

/**
 * The boxes after a top-level step's `invoke:` line: every library, file or
 * plugin gate a Step of this top-level step was inlined through, the one the
 * line names first, then the libraries it invokes in turn, in the Run's order.
 */
export function invokeBoxes<I extends InvokedLike>(
  step: FileStep,
  invoked: I[],
  yanks: YankLike[],
): InvokeBox<I>[] {
  const out: InvokeBox<I>[] = [];
  for (const entry of invoked) {
    const steps = entry.steps.filter((s) => stepRoot(s) === step.id);
    if (steps.length === 0) continue;
    out.push({
      entry,
      steps,
      direct: step.invoke !== null && invokeTarget(entry) === step.invoke,
      yanked:
        entry.kind === "library" && yanks.some((y) => y.name === entry.name && y.content_hash === entry.content_hash),
    });
  }
  return out.sort((a, b) => Number(b.direct) - Number(a.direct));
}

// ── Bar ─────────────────────────────────────────────────────────────────────

/** A file size in words: "820 B", "1.6 KB", "2.3 MB". */
export function sizeWords(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** A ref without its `refs/heads/` or `refs/tags/` prefix. */
export function shortRef(ref: string): string {
  return ref.replace(/^refs\/(heads|tags)\//, "");
}

/** The file's name, for Download. */
export function baseName(path: string): string {
  return path.split("/").pop() || "pipeline.yaml";
}
