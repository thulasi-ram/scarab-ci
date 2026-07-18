// App shell: a slim top bar with the beetle wordmark, search, theme toggle, and
// the signed-in user. Page context lives in the big page header (and, on a run,
// a header-sized breadcrumb) — not in the top bar. Rendered as the Router root
// so it persists across navigations (docs/DESIGN.md §4).
import type { ParentProps } from "solid-js";
import { A } from "@solidjs/router";
import Icon from "./components/Icon";
import UserMenu from "./components/UserMenu";
import CommandPalette from "./components/CommandPalette";
import { setPaletteOpen } from "./palette";
import { theme, toggleTheme } from "./theme";
import emblemLight from "./assets/brand/scarab-emblem.svg";
import emblemDark from "./assets/brand/scarab-emblem-dark.svg";

export default function Layout(props: ParentProps) {
  return (
    <div class="app">
      <nav class="topbar">
        <A href="/" class="brand" end>
          {/* The brand emblem (ui/brand): ink outline on light, gold on dark. */}
          <img
            class="brand-emblem"
            src={theme() === "dark" ? emblemDark : emblemLight}
            alt=""
            width={26}
            height={23}
          />
          <span>Scarab</span>
        </A>
        <span class="topbar-spacer" />
        <button class="search-chip" type="button" onClick={() => setPaletteOpen(true)}>
          <Icon icon="search" size={13} />
          <span class="search-chip-label">Search</span>
          <span class="mono kbd">⌘K</span>
        </button>
        <button
          class="theme-toggle"
          type="button"
          onClick={toggleTheme}
          title={theme() === "dark" ? "Switch to light" : "Switch to dark"}
          aria-label="Toggle theme"
        >
          <Icon icon={theme() === "dark" ? "sun" : "moon"} size={15} />
        </button>
        <UserMenu />
      </nav>
      <CommandPalette />
      {props.children}
    </div>
  );
}
