// App shell: carapace top bar with the beetle wordmark, a contextual breadcrumb
// derived from the route, a search affordance, and the signed-in user. Rendered
// as the Router root so it persists across navigations (docs/DESIGN.md §4).
import type { ParentProps } from "solid-js";
import { For, Show } from "solid-js";
import { A, useLocation } from "@solidjs/router";
import Icon from "./components/Icon";

type Crumb = { label: string; href?: string; mono?: boolean };

export default function Layout(props: ParentProps) {
  const loc = useLocation();

  const crumbs = (): Crumb[] => {
    const seg = loc.pathname.split("/").filter(Boolean);
    if (seg.length === 0) return [{ label: "Repositories" }];
    const out: Crumb[] = [{ label: seg[0], href: `/${seg[0]}` }];
    if (seg[1]) out.push({ label: seg[1], href: `/${seg[0]}/${seg[1]}` });
    if (seg[2] === "runs" && seg[3]) out.push({ label: seg[3].slice(0, 7), mono: true });
    return out;
  };

  return (
    <div class="app">
      <nav class="topbar">
        <A href="/" class="brand" end>
          <Icon icon="bug" size={19} />
          <span>Scarab</span>
        </A>
        <span class="crumbs">
          <For each={crumbs()}>
            {(c, i) => (
              <>
                <Show when={i() > 0}>
                  <Icon icon="chevron-right" size={13} class="crumb-sep" />
                </Show>
                <Show when={c.href} fallback={<span class={`crumb ${c.mono ? "mono" : ""}`}>{c.label}</span>}>
                  <A href={c.href!} class={`crumb ${c.mono ? "mono" : ""}`}>
                    {c.label}
                  </A>
                </Show>
              </>
            )}
          </For>
        </span>
        <span class="topbar-spacer" />
        <button class="search-chip" type="button">
          <Icon icon="search" size={13} />
          <span class="mono">⌘K</span>
        </button>
        <span class="avatar" title="t.ram" />
      </nav>
      {props.children}
    </div>
  );
}
