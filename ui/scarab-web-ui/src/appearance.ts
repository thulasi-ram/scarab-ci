// Appearance choices that are pure data: which palettes exist, and how a
// stored value is read back. No DOM here, so the no-DOM vitest tier covers it;
// `theme.ts` applies the choice to <html>.
//
// The palette is independent of light/dark. Emerald is the default look; Mono
// turns surfaces, text, borders and the accent to greys, and keeps the status
// colours as hues so success, failure, running and waiting still read apart.
// index.html repeats the same rule in its pre-paint guard (it runs before any
// module loads), so the two must agree on KEY and on the accepted values.

export type Palette = "emerald" | "mono";

export const PALETTE_KEY = "scarab-palette";

export const PALETTES: readonly { id: Palette; label: string; sub: string }[] = [
  { id: "emerald", label: "Emerald", sub: "green accent, gold edges" },
  { id: "mono", label: "Mono", sub: "grays only; status colors stay" },
];

/** A stored or attribute value as a palette. Anything unknown is the default. */
export function parsePalette(raw: string | null | undefined): Palette {
  return raw === "mono" ? "mono" : "emerald";
}
