// App shell: a slim top bar with the beetle wordmark, search, theme toggle, and
// the signed-in user. Page context lives in the big page header (and, on a run,
// a header-sized breadcrumb) — not in the top bar. Rendered as the Router root
// so it persists across navigations (docs/DESIGN.md §4).
import { Show, type ParentProps } from "solid-js";
import { A } from "@solidjs/router";
import Icon from "./components/Icon";
import Login from "./Login";
import { unauthenticated } from "./api/client";
import UserMenu from "./components/UserMenu";
import CommandPalette from "./components/CommandPalette";
import { setPaletteOpen } from "./palette";
import { canAdminister } from "./session";
import { theme, toggleTheme } from "./theme";
import emblemGold from "./assets/brand/scarab-emblem-dark.svg";

export default function Layout(props: ParentProps) {
  // No session => the sign-in screen INSTEAD of the shell, for every route at
  // once. Rendering the chrome around a login form would be a lie: the search
  // palette, the settings entry and the identity menu all need a principal, and
  // every one of them would sit there failing quietly.
  //
  // <Show>, not an early `return` — a Solid component body runs ONCE, so a bare
  // `if (unauthenticated())` reads the signal outside any reactive scope and
  // the shell would never flip when a session expires mid-visit.
  return (
    <Show when={!unauthenticated()} fallback={<Login />}>
    <div class="app">
      <nav class="topbar">
        <A href="/" class="brand" end>
          {/* The brand emblem (ui/brand): gold carapace in both themes. */}
          <img
            class="brand-emblem"
            src={emblemGold}
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
        {/* Org settings (ADR-0060): hidden outright from non-admins — nothing
            in there is actionable or informative without `Administer`. */}
        <Show when={canAdminister()}>
          <A href="/settings" class="topbar-icon" title="Settings" aria-label="Settings">
            <Icon icon="settings" size={15} />
          </A>
        </Show>
        <button
          class="theme-toggle"
          type="button"
          onClick={toggleTheme}
          title={theme() === "dark" ? "Switch to light" : "Switch to dark"}
          aria-label="Toggle theme"
        >
          {/* Both rendered, stacked; CSS crossfades/rotates on theme change. */}
          <Icon icon="sun" size={15} class="tt-icon tt-sun" />
          <Icon icon="moon" size={15} class="tt-icon tt-moon" />
        </button>
        <UserMenu />
      </nav>
      <CommandPalette />
      {props.children}
    </div>
    </Show>
  );
}
