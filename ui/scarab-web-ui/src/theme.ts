// Theme state (docs/DESIGN.md §1). Two independent choices, both stored in
// localStorage and applied to <html> as attributes:
//
// - mode: light (the default) or dark, as `data-theme`;
// - palette: emerald (the default) or mono, as `data-palette`.
//
// The pre-paint guard in index.html sets both attributes before first paint to
// avoid a flash; this module keeps reactive signals in sync so the controls
// re-render.
import { createSignal } from "solid-js";
import { PALETTE_KEY, parsePalette, type Palette } from "./appearance";

export type Theme = "light" | "dark";
export type { Palette };
const KEY = "scarab-theme";

function initial(): Theme {
  const attr = document.documentElement.getAttribute("data-theme");
  return attr === "dark" ? "dark" : "light";
}

const [theme, setThemeSignal] = createSignal<Theme>(initial());
const [palette, setPaletteSignal] = createSignal<Palette>(
  parsePalette(document.documentElement.getAttribute("data-palette")),
);

export { theme, palette };

export function setTheme(next: Theme) {
  document.documentElement.setAttribute("data-theme", next);
  try {
    localStorage.setItem(KEY, next);
  } catch {
    /* private mode / storage disabled — in-memory only */
  }
  setThemeSignal(next);
}

export function toggleTheme() {
  setTheme(theme() === "dark" ? "light" : "dark");
}

export function setPalette(next: Palette) {
  document.documentElement.setAttribute("data-palette", next);
  try {
    localStorage.setItem(PALETTE_KEY, next);
  } catch {
    /* private mode / storage disabled — in-memory only */
  }
  setPaletteSignal(next);
}
