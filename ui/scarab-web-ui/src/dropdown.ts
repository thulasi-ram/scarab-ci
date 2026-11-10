// The list logic behind the one dropdown (`components/SearchSelect.tsx`):
// filtering, which row the arrow keys and Home/End land on, and type-ahead.
// Pure, so it is tested without a DOM.

/** The fields of an option this logic reads. */
export interface DropdownRow {
  label: string;
  disabled?: boolean;
}

/** Above this many options the dropdown shows a search box. */
export const SEARCH_THRESHOLD = 8;

/** Whether the search box shows: the caller's choice, else the list's length. */
export function showsSearch(count: number, search: boolean | undefined): boolean {
  return search ?? count > SEARCH_THRESHOLD;
}

/** The options whose label contains the query, case-insensitively. */
export function filterRows<T extends DropdownRow>(rows: T[], query: string): T[] {
  const q = query.trim().toLowerCase();
  return q ? rows.filter((r) => r.label.toLowerCase().includes(q)) : rows;
}

/**
 * The next row that can be picked from `from` in direction `step` (+1 down,
 * -1 up), skipping disabled rows. It stops at the ends: past the last
 * enabled row it stays where it is. `from` of -1 with step +1 finds the
 * first enabled row.
 */
export function stepIndex(rows: DropdownRow[], from: number, step: 1 | -1): number {
  for (let i = from + step; i >= 0 && i < rows.length; i += step) {
    if (!rows[i].disabled) return i;
  }
  return from;
}

/** The first row that can be picked, or -1 when none can. */
export function firstEnabled(rows: DropdownRow[]): number {
  return rows.findIndex((r) => !r.disabled);
}

/** The last row that can be picked, or -1 when none can. */
export function lastEnabled(rows: DropdownRow[]): number {
  for (let i = rows.length - 1; i >= 0; i--) if (!rows[i].disabled) return i;
  return -1;
}

/**
 * The row a list opens on: the selected one when it can be picked, else the
 * first that can.
 */
export function openIndex(rows: DropdownRow[], selected: number): number {
  return selected >= 0 && selected < rows.length && !rows[selected].disabled ? selected : firstEnabled(rows);
}

/**
 * Type-ahead, as a native select does it: the next enabled row whose label
 * starts with what was typed. A buffer of one repeated letter ("bbb") cycles
 * through the rows starting with that letter, starting after `from`; a longer
 * buffer ("bet") matches from `from` itself, so typing on keeps the row.
 * Returns -1 when nothing matches.
 */
export function typeAheadIndex(rows: DropdownRow[], buffer: string, from: number): number {
  if (!buffer || rows.length === 0) return -1;
  const b = buffer.toLowerCase();
  const repeated = [...b].every((c) => c === b[0]);
  const needle = repeated ? b[0] : b;
  const start = repeated ? from + 1 : Math.max(from, 0);
  for (let k = 0; k < rows.length; k++) {
    const i = (((start + k) % rows.length) + rows.length) % rows.length;
    const r = rows[i];
    if (!r.disabled && r.label.toLowerCase().startsWith(needle)) return i;
  }
  return -1;
}
