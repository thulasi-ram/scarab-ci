// Pure DAG layering (extracted from components/Dag.tsx): steps lay out in
// dependency layers by longest-path depth from the roots — a step sits one
// column right of its deepest dependency. Cycles (validated away server-side)
// are guarded so a malformed graph can't hang the layout.

/** Group `steps` into dependency columns: `result[d]` holds every step whose
 * longest `needs` chain from a root has length `d`. Column order follows the
 * input order within a layer; empty layers are dropped. A `needs` entry naming
 * an unknown step contributes depth 0 (treated as an external root). */
export function layerSteps<T extends { id: string; needs: string[] }>(steps: T[]): T[][] {
  const byId = new Map(steps.map((s) => [s.id, s]));
  const memo = new Map<string, number>();
  const depth = (id: string, seen: Set<string>): number => {
    if (memo.has(id)) return memo.get(id)!;
    if (seen.has(id)) return 0; // cycle guard (validated away server-side)
    seen.add(id);
    const s = byId.get(id);
    const d = !s || s.needs.length === 0 ? 0 : 1 + Math.max(...s.needs.map((n) => depth(n, seen)));
    seen.delete(id);
    memo.set(id, d);
    return d;
  };
  const cols: T[][] = [];
  for (const s of steps) {
    const d = depth(s.id, new Set());
    (cols[d] ||= []).push(s);
  }
  return cols.filter((c) => c && c.length);
}

// ── Invocation groups (ADR-0076) ───────────────────────────────────────────
// A Step inlined from a library (or a `file:` library) carries the invoke
// chain it came through (`origin`, outermost first), and its id is the chain
// of invoke Step ids joined by `/` (`ship/smoke/probe`). The DAG draws one
// box per invocation around its Steps, nested for an inner invocation: each
// box is one unit in its parent's layering, and its own Steps are layered
// inside it. A plugin gate's origin draws no box: it is one node.

/** One link of a Step's invoke chain, as the Run API returns it. */
export type OriginLink = {
  kind: string;
  name: string;
  content_hash: string;
  source?: string | null;
  yanked?: boolean;
};

/** One invocation box. */
export type DagGroup = {
  /** The invoke Step's id in the Run (`ship`, `ship/smoke`). */
  key: string;
  /** The invoke Step's own id (`smoke`). */
  label: string;
  /** `library` or `file`. */
  kind: string;
  /** `<publisher>/<name>` or the file path. */
  name: string;
  content_hash: string;
  yanked: boolean;
};

/** A layout cell: a Step, or a box with its own columns. */
export type DagCell<T> = { kind: "step"; step: T } | { kind: "group"; group: DagGroup; cols: DagCell<T>[][] };

/** A Step id's `/`-separated parts, keeping a matrix leg's `[…]` whole. */
export function idSegments(id: string): string[] {
  const out: string[] = [];
  let cur = "";
  let depth = 0;
  for (const c of id) {
    if (c === "[") depth++;
    if (c === "]") depth = Math.max(0, depth - 1);
    if (c === "/" && depth === 0) {
      out.push(cur);
      cur = "";
    } else cur += c;
  }
  out.push(cur);
  return out;
}

/** The invocation boxes a Step sits in, outermost first. */
export function stepGroups(s: { id: string; origin?: OriginLink[] | null }): DagGroup[] {
  const links = (s.origin ?? []).filter((o) => o.kind === "library" || o.kind === "file");
  const segs = idSegments(s.id);
  const n = Math.min(links.length, segs.length - 1);
  const out: DagGroup[] = [];
  for (let i = 0; i < n; i++) {
    const o = links[i]!;
    out.push({
      key: segs.slice(0, i + 1).join("/"),
      label: segs[i]!,
      kind: o.kind,
      name: o.name,
      content_hash: o.content_hash,
      yanked: !!o.yanked,
    });
  }
  return out;
}

/** `steps` laid out in dependency columns, each invocation's Steps in a box
 * that is one unit of its parent's columns. With no invocation it is
 * [`layerSteps`] with every Step a `step` cell. */
export function layoutGroups<T extends { id: string; needs: string[]; origin?: OriginLink[] | null }>(
  steps: T[],
): DagCell<T>[][] {
  const groups = new Map(steps.map((s) => [s.id, stepGroups(s)]));
  const level = (items: T[], depth: number): DagCell<T>[][] => {
    type Unit = { id: string; needs: string[]; members: T[]; group?: DagGroup };
    const units = new Map<string, Unit>();
    const unitOf = new Map<string, string>();
    for (const s of items) {
      const g = groups.get(s.id)![depth];
      const key = g ? `g:${g.key}` : `s:${s.id}`;
      let u = units.get(key);
      if (!u) {
        u = { id: key, needs: [], members: [], group: g };
        units.set(key, u);
      }
      u.members.push(s);
      unitOf.set(s.id, key);
    }
    for (const u of units.values()) {
      const needs = new Set<string>();
      for (const m of u.members) {
        for (const n of m.needs) {
          const nu = unitOf.get(n);
          if (nu && nu !== u.id) needs.add(nu);
        }
      }
      u.needs = [...needs];
    }
    return layerSteps([...units.values()]).map((col) =>
      col.map((u): DagCell<T> =>
        u.group
          ? { kind: "group", group: u.group, cols: level(u.members, depth + 1) }
          : { kind: "step", step: u.members[0]! },
      ),
    );
  };
  return level(steps, 0);
}
