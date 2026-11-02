// The repo Secrets list: the repo's own keys, then the keys it can use but does
// not hold. Built from the repo-scope names (`GET /v1/secrets`) and the secret
// matrix, which already carries what the org holds. Names only, never values.
import { REPO_DEFAULT_COLUMN, type SecretMatrix } from "./api/client";

export type SecretKeyRow =
  /** Held at the repo scope. `overridesOrg` when the org holds the same key. */
  | { name: string; kind: "own"; overridesOrg: boolean }
  /** Not at the repo scope; resolves from the org. */
  | { name: string; kind: "org" }
  /** Only in some environments' scopes, so only Runs in those environments see it. */
  | { name: string; kind: "env"; environments: string[] };

const byName = (a: { name: string }, b: { name: string }) =>
  a.name < b.name ? -1 : a.name > b.name ? 1 : 0;

/**
 * Every key a repo's Runs may resolve. The repo's own keys come first, sorted;
 * then the org's keys the repo does not override, then keys that live only in
 * environments. Without a matrix (it failed to load) the list is the own keys.
 */
export function secretKeyRows(own: string[], matrix: SecretMatrix | null | undefined): SecretKeyRow[] {
  const ownSet = new Set(own);
  const rows = matrix?.keys ?? [];
  const at = (key: string) => rows.find((r) => r.key === key);

  const mine: SecretKeyRow[] = [...ownSet]
    .map((name) => ({
      name,
      kind: "own" as const,
      overridesOrg: at(name)?.overrides?.[REPO_DEFAULT_COLUMN] === "org",
    }))
    .sort(byName);

  const fromOrg: SecretKeyRow[] = [];
  const envOnly: SecretKeyRow[] = [];
  for (const r of rows) {
    if (ownSet.has(r.key)) continue;
    if (
      r.status[REPO_DEFAULT_COLUMN] === "inherited" &&
      r.inherited_from[REPO_DEFAULT_COLUMN] === "org"
    ) {
      fromOrg.push({ name: r.key, kind: "org" });
      continue;
    }
    const environments = (matrix?.environments ?? []).filter((e) => r.status[e] === "set");
    if (environments.length > 0) envOnly.push({ name: r.key, kind: "env", environments });
  }
  return [...mine, ...fromOrg.sort(byName), ...envOnly.sort(byName)];
}

/** The short note beside a row; empty for a repo key that hides nothing. */
export function secretKeyNote(row: SecretKeyRow): string {
  switch (row.kind) {
    case "own":
      return row.overridesOrg ? "overrides org" : "";
    case "org":
      return "from org";
    case "env":
      return `only in ${row.environments.join(", ")}`;
  }
}
