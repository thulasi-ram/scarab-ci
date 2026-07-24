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
