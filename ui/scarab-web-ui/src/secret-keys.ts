// The Secrets key lists, as pure functions over a secret matrix: one row per
// key with a one-line summary of where it is set, and on expand one line per
// scope (the default, then each Environment). A repository's list has the
// Repository default; the org's list (Org settings › Secrets) has the Org
// default and says which repositories set their own value in front of each
// line. Names and states only; the matrix never carries a value.
import {
  REPO_DEFAULT_COLUMN,
  type SecretCellStatus,
  type SecretMatrix,
} from "./api/client";

export type SecretKeyRow = SecretMatrix["keys"][number];

/** A scope's name as the list shows it: the default scope in the user's words. */
export const scopeLabel = (column: string, org = false) =>
  column === REPO_DEFAULT_COLUMN ? (org ? "Org default" : "Repository default") : column;

export const GLYPH: Record<SecretCellStatus, string> = {
  set: "●",
  inherited: "○",
  unset: "–",
  silenced: "⊘",
};

/** A cell's status; a column the server left out resolves to nothing. */
export const statusAt = (row: SecretKeyRow, column: string): SecretCellStatus =>
  row.status[column] ?? "unset";

/**
 * A Run of this repository gets the org's value for the key somewhere: the
 * repository default comes from the org, or, with no repository default, an
 * Environment gets the org's value for it. A value the repository sets in
 * front of the org's does not count: there the Run gets the repository's.
 */
export const isOrgKey = (row: SecretKeyRow) =>
  (statusAt(row, REPO_DEFAULT_COLUMN) === "inherited" &&
    row.inherited_from[REPO_DEFAULT_COLUMN] === "org") ||
  (statusAt(row, REPO_DEFAULT_COLUMN) !== "set" &&
    Object.values(row.inherited_from).includes("org_environment"));

/**
 * The org's list: who sets their own value in front of one line —
 * "web sets its own", "web and api set their own", "3 repositories set their own".
 */
export function ownText(repos: string[]): string {
  if (repos.length === 1) return `${repos[0]} sets its own`;
  if (repos.length === 2) return `${repos[0]} and ${repos[1]} set their own`;
  return `${repos.length} repositories set their own`;
}

const ownAt = (row: SecretKeyRow, column: string) => row.repos_with_own?.[column] ?? [];

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

/** What one scope line says after its glyph. `org` is the org's own list. */
export function stateText(row: SecretKeyRow, column: string, org = false): string {
  if (org) {
    const own = ownAt(row, column);
    const base =
      statusAt(row, column) === "set"
        ? "set here"
        : statusAt(row, column) === "inherited"
          ? "inherited (Org default)"
          : "unset";
    return own.length > 0 ? `${base} · ${ownText(own)}` : base;
  }
  switch (statusAt(row, column)) {
    case "set": {
      const over = row.overrides?.[column];
      if (over === "org_environment") return `set here, overrides the org value for ${column}`;
      if (over === "org") return "set here, overrides the org value";
      return "set here";
    }
    case "inherited": {
      const from = row.inherited_from[column];
      if (from === "org_environment") return `inherited (org value for ${column})`;
      return `inherited (${from === "org" ? "org" : "Repository default"})`;
    }
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
export function keySummary(matrix: SecretMatrix, row: SecretKeyRow, org = false): string {
  if (org) return orgKeySummary(matrix, row);
  const envs = matrix.environments;
  const at = (s: SecretCellStatus) => envs.filter((e) => statusAt(row, e) === s);
  const set = at("set");
  const silenced = at("silenced");
  // Where a Run gets the org's value for the Environment.
  const fromOrgEnv = envs.filter(
    (e) => statusAt(row, e) === "inherited" && row.inherited_from[e] === "org_environment",
  );
  // A value set here hides another one: the default's, or the org's.
  const hidesOne = set.some((e) => row.overrides?.[e] !== undefined);
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
    if (fromOrgEnv.length > 0) parts.push(`from the org in ${where(fromOrgEnv, envs.length)}`);
    if (set.length > 0) {
      parts.push(
        `${resolves || hidesOne ? "overridden in" : "set in"} ${where(set, envs.length)}`,
      );
    } else if (base === "inherited" && fromOrgEnv.length === 0) {
      parts.push("inherited everywhere");
    }
    if (silenced.length > 0) parts.push(`unset on purpose in ${where(silenced, envs.length)}`);
  }
  return parts.join(" · ");
}

/**
 * The org list's collapsed summary: where the org sets the key, then which
 * repositories set their own in front of it.
 */
function orgKeySummary(matrix: SecretMatrix, row: SecretKeyRow): string {
  const envs = matrix.environments;
  const set = envs.filter((e) => statusAt(row, e) === "set");
  const parts = [statusAt(row, REPO_DEFAULT_COLUMN) === "set" ? "default set" : "unset by default"];
  if (set.length > 0) parts.push(`set in ${where(set, envs.length)}`);
  const columns = scopeColumns(matrix).filter((c) => ownAt(row, c).length > 0);
  const repos = [...new Set(columns.flatMap((c) => ownAt(row, c)))].sort();
  if (repos.length > 0) {
    const only = columns.length === 1 && columns[0] !== REPO_DEFAULT_COLUMN ? ` for ${columns[0]}` : "";
    parts.push(`${ownText(repos)}${only}`);
  }
  return parts.join(" · ");
}

/**
 * The collapsed row's summary with one Environment picked: what a Run in it
 * gets for this key, and where that comes from.
 */
export function envSummary(row: SecretKeyRow, environment: string, org = false): string {
  if (org) {
    const own = ownAt(row, environment);
    const base =
      statusAt(row, environment) === "set"
        ? "set here"
        : statusAt(row, environment) === "inherited"
          ? "from the Org default"
          : "unset";
    return own.length > 0 ? `${base} · ${ownText(own)}` : base;
  }
  switch (statusAt(row, environment)) {
    case "set": {
      const over = row.overrides?.[environment];
      return over === "repo"
        ? "set here, overrides the Repository default"
        : over === "org_environment"
          ? `set here, overrides the org value for ${environment}`
          : over === "org"
            ? "set here, overrides the org value"
            : "set here";
    }
    case "inherited": {
      const from = row.inherited_from[environment];
      return from === "org_environment"
        ? `from the org value for ${environment}`
        : from === "org"
          ? "from the org"
          : "from the Repository default";
    }
    case "unset":
      return "unset";
    case "silenced":
      return "unset on purpose";
  }
}
