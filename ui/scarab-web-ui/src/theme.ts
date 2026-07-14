// Theme state (docs/DESIGN.md §1). Light is the default; dark is the opt-in
// carapace. The choice is stored in localStorage and applied to <html> as
// `data-theme`. The pre-paint guard in index.html sets the attribute before
// first paint to avoid a flash; this module keeps a reactive signal in sync so
// the toggle button re-renders.
import { createSignal } from "solid-js";

export type Theme = "light" | "dark";
const KEY = "scarab-theme";

function initial(): Theme {
  const attr = document.documentElement.getAttribute("data-theme");
  return attr === "dark" ? "dark" : "light";
}

const [theme, setThemeSignal] = createSignal<Theme>(initial());

export { theme };

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
