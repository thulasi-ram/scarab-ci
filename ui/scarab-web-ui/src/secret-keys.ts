// The repo Secrets key list, as pure functions over the secret matrix: one row
// per key with a one-line summary of where it is set, and on expand one line
// per scope (the Repository default, then each Environment). Names and states
// only; the matrix never carries a value.
import {
  REPO_DEFAULT_COLUMN,
  type SecretCellStatus,
  type SecretMatrix,
} from "./api/client";

export type SecretKeyRow = SecretMatrix["keys"][number];

/** A scope's name as the list shows it: the repo scope in the user's words. */
export const scopeLabel = (column: string) =>
  column === REPO_DEFAULT_COLUMN ? "Repository default" : column;

export const GLYPH: Record<SecretCellStatus, string> = {
  set: "●",
  inherited: "○",
  unset: "–",
  silenced: "⊘",
};

/** A cell's status; a column the server left out resolves to nothing. */
export const statusAt = (row: SecretKeyRow, column: string): SecretCellStatus =>
  row.status[column] ?? "unset";

/** The repo default resolves from the org: the org holds the key, the repo does not. */
export const isOrgKey = (row: SecretKeyRow) =>
  statusAt(row, REPO_DEFAULT_COLUMN) === "inherited" &&
  row.inherited_from[REPO_DEFAULT_COLUMN] === "org";

/** The matrix's keys, sorted by name. */
export const sortedKeys = (matrix: SecretMatrix): SecretKeyRow[] =>
  [...matrix.keys].sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));

/** The scope columns in render order: the Repository default first. */
export const scopeColumns = (matrix: SecretMatrix): string[] => [
  REPO_DEFAULT_COLUMN,
  ...matrix.columns.filter((c) => c !== REPO_DEFAULT_COLUMN),
];

/** The names that hold a value at exactly this scope (the overwrite guard's list). */
export const namesSetAt = (matrix: SecretMatrix, column: string): string[] =>
  matrix.keys.filter((r) => statusAt(r, column) === "set").map((r) => r.key);

/**
 * A matrix holding only the repo scope's names, for when the matrix itself
 * cannot be read: the list still shows the repo's own keys.
 */
export const matrixFromNames = (names: string[]): SecretMatrix => ({
  columns: [REPO_DEFAULT_COLUMN],
  environments: [],
  keys: names.map((key) => ({
    key,
    status: { [REPO_DEFAULT_COLUMN]: "set" },
    inherited_from: {},
    overrides: {},
  })),
});

/** What one scope line says after its glyph. */
export function stateText(row: SecretKeyRow, column: string): string {
  switch (statusAt(row, column)) {
    case "set": {
      const over = row.overrides?.[column];
      if (over === "org") return "set here, overrides the org value";
      return "set here";
    }
    case "inherited":
      return `inherited (${row.inherited_from[column] === "org" ? "org" : "Repository default"})`;
    case "unset":
      return "unset";
    case "silenced":
      return "unset on purpose";
  }
}

export type ScopeLines = {
  /** Always shown: the Repository default, scopes set or unset on purpose here, the pinned one. */
  shown: string[];
  /** Inherited or unset scopes, behind a "N more" line. Empty when there is nothing to fold. */
  folded: string[];
};

/**
 * Which scope lines show on expand, and which fold away. A single foldable
 * line is shown rather than folded: the fold line would take its place.
 */
export function scopeLines(matrix: SecretMatrix, row: SecretKeyRow, pinned?: string): ScopeLines {
  const columns = scopeColumns(matrix);
  const keep = (c: string) => {
    const s = statusAt(row, c);
    return c === REPO_DEFAULT_COLUMN || c === pinned || s === "set" || s === "silenced";
  };
  const folded = columns.filter((c) => !keep(c));
  if (folded.length < 2) return { shown: columns, folded: [] };
  return { shown: columns.filter(keep), folded };
}

/** The fold line's words: "8 more inherited", "3 more unset", or both. */
export function foldLabel(row: SecretKeyRow, folded: string[]): string {
  const inherited = folded.filter((c) => statusAt(row, c) === "inherited").length;
  const what =
    inherited === folded.length ? "inherited" : inherited === 0 ? "unset" : "inherited or unset";
  return `${folded.length} more ${what}`;
}

/** "production", "production and qa", or "3 of 12". */
function where(names: string[], total: number): string {
  if (names.length === 1) return names[0];
  if (names.length === 2) return `${names[0]} and ${names[1]}`;
  return `${names.length} of ${total}`;
}

/**
 * The collapsed row's summary when no Environment is picked: where the key is
 * set, read from the Repository default outwards. The `org` tag beside the row
 * says when the default comes from the org.
 */
export function keySummary(matrix: SecretMatrix, row: SecretKeyRow): string {
  const envs = matrix.environments;
  const at = (s: SecretCellStatus) => envs.filter((e) => statusAt(row, e) === s);
  const set = at("set");
  const silenced = at("silenced");
  const parts: string[] = [];

  const base = statusAt(row, REPO_DEFAULT_COLUMN);
  const resolves = base === "set" || base === "inherited";
  if (base === "set") {
    parts.push(row.overrides?.[REPO_DEFAULT_COLUMN] === "org" ? "default set, overrides org" : "default set");
  } else if (base === "inherited") {
    if (envs.length === 0) parts.push("from org");
  } else if (base === "silenced") {
    parts.push("unset on purpose by default");
  } else {
    parts.push("unset by default");
  }

  if (envs.length > 0) {
    if (set.length > 0) {
      parts.push(`${resolves ? "overridden in" : "set in"} ${where(set, envs.length)}`);
    } else if (base === "inherited") {
      parts.push("inherited everywhere");
    }
    if (silenced.length > 0) parts.push(`unset on purpose in ${where(silenced, envs.length)}`);
  }
  return parts.join(" · ");
}

/**
 * The collapsed row's summary with one Environment picked: what a Run in it
 * gets for this key, and where that comes from.
 */
export function envSummary(row: SecretKeyRow, environment: string): string {
  switch (statusAt(row, environment)) {
    case "set": {
      const over = row.overrides?.[environment];
      return over === "repo"
        ? "set here, overrides the Repository default"
        : over === "org"
          ? "set here, overrides the org value"
          : "set here";
    }
    case "inherited":
      return row.inherited_from[environment] === "org" ? "from the org" : "from the Repository default";
    case "unset":
      return "unset";
    case "silenced":
      return "unset on purpose";
  }
}
