// The signed-in identity (ADR-0049): an avatar showing initials, opening a
// dropdown with the principal's name, subject, and role, plus sign-out. Driven
// by the real `GET /v1/me` — no more hardcoded placeholder.
import { createSignal, onCleanup, onMount, Show } from "solid-js";
import { A } from "@solidjs/router";
import Icon from "./Icon";
import { logout } from "../api/client";
import { canAdminister, me } from "../session";
import { theme, toggleTheme } from "../theme";

/** Initials from a display name ("Thulasi Ram" → "TR") or subject ("t.ram" → "T"). */
function initials(name: string): string {
  const parts = name.trim().split(/[\s._-]+/).filter(Boolean);
  if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase();
  return (parts[0]?.[0] ?? "?").toUpperCase();
}

export default function UserMenu() {
  const [open, setOpen] = createSignal(false);
  let root: HTMLDivElement | undefined;

  const label = () => me()?.display_name || me()?.subject || "";

  onMount(() => {
    const onDoc = (e: MouseEvent) => {
      if (root && !root.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("click", onDoc);
    document.addEventListener("keydown", onKey);
    onCleanup(() => {
      document.removeEventListener("click", onDoc);
      document.removeEventListener("keydown", onKey);
    });
  });

  return (
    <div class="usermenu" ref={root}>
      <button
        class="avatar"
        type="button"
        title={label()}
        aria-haspopup="menu"
        aria-expanded={open()}
        onClick={() => setOpen((v) => !v)}
      >
        <Show when={label()} fallback={<span class="avatar-init">·</span>}>
          <span class="avatar-init">{initials(label())}</span>
        </Show>
      </button>
      <Show when={open() && me()}>
        {(user) => (
          <div class="usermenu-pop" role="menu">
            <div class="um-head">
              <span class="avatar um-avatar" aria-hidden="true">
                <span class="avatar-init">{initials(label())}</span>
              </span>
              <div class="um-id">
                <div class="um-name">{user().display_name ?? user().subject}</div>
                <div class="um-sub mono">{user().subject}</div>
              </div>
            </div>
            <div class="um-roles">
              {user().roles.map((r) => (
                <span class="um-role">{r}</span>
              ))}
            </div>
            {/* Per-account controls, moved off the top bar. Settings stays
                hidden outright from non-admins (ADR-0060): nothing in there is
                actionable or informative without `Administer`. */}
            <Show when={canAdminister()}>
              <A
                href="/settings"
                class="um-item um-link"
                role="menuitem"
                onClick={() => setOpen(false)}
              >
                <Icon icon="settings" size={14} />
                Settings
              </A>
            </Show>
            {/* Says what the click DOES, not what the theme currently is — the
                top-bar toggle it replaces could lean on a sun/moon crossfade
                to carry that, and a text row cannot. Leaves the menu open, so
                the change is visible where it was made. */}
            <button
              class="um-item"
              type="button"
              role="menuitem"
              onClick={toggleTheme}
            >
              <Icon icon={theme() === "dark" ? "sun" : "moon"} size={14} />
              {theme() === "dark" ? "Switch to light" : "Switch to dark"}
            </button>
            {/* The icon set carries no sign-out glyph, so this row keeps an
                empty slot of the same width rather than letting its label sit
                a gap to the left of the two above it. */}
            <button class="um-item" type="button" role="menuitem" onClick={() => void logout()}>
              <span class="um-icon-slot" aria-hidden="true" />
              Sign out
            </button>
          </div>
        )}
      </Show>
    </div>
  );
}
