// How the Results tab shows one step result's value. A number, string,
// boolean or null shows in full on its row. An object or a list shows as one
// line (how many keys or items, then a few of its plain values) and expands to
// the whole value as formatted JSON.

export type ResultView =
  | { kind: "plain"; text: string }
  | { kind: "tree"; size: string; preview: string; json: string };

/** Keys read first in a preview: the ones a reader looks for in an image or a
 *  push result. The rest follow in the object's own order. */
const FIRST = ["digest", "ref", "image.name", "name", "url", "version", "tag"];

/** How many plain values a preview names before it stops. */
const PREVIEW_ENTRIES = 2;

const plain = (v: unknown): v is string | number | boolean | null =>
  v === null || typeof v === "string" || typeof v === "number" || typeof v === "boolean";

const text = (v: string | number | boolean | null) => (typeof v === "string" ? v : JSON.stringify(v));

export function resultView(v: unknown): ResultView {
  if (plain(v)) return { kind: "plain", text: text(v) };
  if (v === undefined) return { kind: "plain", text: "" };
  const json = JSON.stringify(v, null, 2);
  if (Array.isArray(v)) {
    const preview = v.filter(plain).slice(0, PREVIEW_ENTRIES + 1).map(text).join(", ");
    return { kind: "tree", size: `${v.length} ${v.length === 1 ? "item" : "items"}`, preview, json };
  }
  const entries = Object.entries(v as Record<string, unknown>);
  const rank = (k: string) => {
    const i = FIRST.indexOf(k);
    return i < 0 ? FIRST.length : i;
  };
  const shown = entries
    .map(([k, x], i) => ({ k, x, i }))
    .filter((e) => plain(e.x))
    .sort((a, b) => rank(a.k) - rank(b.k) || a.i - b.i)
    .slice(0, PREVIEW_ENTRIES)
    .map((e) => `${e.k}: ${text(e.x as string | number | boolean | null)}`)
    .join(" · ");
  return { kind: "tree", size: `${entries.length} ${entries.length === 1 ? "key" : "keys"}`, preview: shown, json };
}
