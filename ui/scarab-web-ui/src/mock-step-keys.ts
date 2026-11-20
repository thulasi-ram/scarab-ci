/** Step keys for the mock fixtures. A real server mints a random key per
 * step; the fixture derives one from the step id (FNV-1a), so a key is the
 * same on every load and the mock routes can map it back to the id. */

const ALPHABET = "abcdefghijklmnopqrstuvwxyz234567";
const ids = new Map<string, string>();

/** The fixture key of the step `id`: 12 characters of lowercase base32. */
export function mockStepKey(id: string): string {
  let h = 0x811c9dc5;
  let key = "";
  for (let i = 0; i < 12; i++) {
    for (const c of `${i}:${id}`) h = Math.imul(h ^ c.charCodeAt(0), 0x01000193) >>> 0;
    key += ALPHABET[h & 31];
  }
  ids.set(key, id);
  return key;
}

/** The step id a `{step}` path segment names in the mock: the id whose key
 * it is, else the decoded segment itself (an authored id). */
export function mockStepId(segment: string): string {
  const s = decodeURIComponent(segment);
  return ids.get(s) ?? s;
}

/** `run` with a key on every step that has none. */
export function withStepKeys<S extends { id: string; key?: string }, R extends { steps: S[] }>(
  run: R,
): Omit<R, "steps"> & { steps: (S & { key: string })[] } {
  return { ...run, steps: run.steps.map((s) => ({ ...s, key: s.key ?? mockStepKey(s.id) })) };
}
