// App shell: a slim top bar with the beetle wordmark, search, theme toggle, and
// the signed-in user. Page context lives in the big page header (and, on a run,
// a header-sized breadcrumb) — not in the top bar. Rendered as the Router root
// so it persists across navigations (docs/DESIGN.md §4).
import { Show, createResource, type ParentProps } from "solid-js";
import { A } from "@solidjs/router";
import Icon from "./components/Icon";
import UserMenu from "./components/UserMenu";
import CommandPalette from "./components/CommandPalette";
import StepUpDialog from "./components/StepUpDialog";
import { setPaletteOpen } from "./palette";
import { me } from "./session";
import { getOrgSecuritySummary } from "./api/client";
import { bannerText } from "./security";
import emblemGold from "./assets/brand/scarab-emblem-dark.svg";

export default function Layout(props: ParentProps) {
  // Step-up posture for callers who administer an Org (ADR-0073): a crown
  // jewel is refused for every admin with no passkey, so say so app-wide
  // until it is fixed. Nobody else is asked for the summary at all.
  const [security] = createResource(() => me()?.admin_orgs[0], getOrgSecuritySummary);
  const banner = () => bannerText(security());

  return (
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
        {/* The two things a visitor most often wants next to the product
            itself: how it works, and how to drive it. Both leave the SPA, so
            both are plain anchors opening a new tab — `/openapi.json` is a
            SERVER route and a router link would try to match it against the
            app's routes and render nothing. Settings and the theme toggle used
            to sit here; they moved into the user menu, which is where
            per-account controls belong and where they stop competing with
            these two for the eye. */}
        <a
          class="topbar-link"
          href="https://thulasi-ram.github.io/scarab-ci/"
          target="_blank"
          rel="noreferrer"
        >
          Docs
        </a>
        <a class="topbar-link" href="/openapi.json" target="_blank" rel="noreferrer">
          API
        </a>
        <UserMenu />
      </nav>
      <Show when={banner()}>
        <div class="sec-banner" role="status">
          <Icon icon="shield-check" size={13} />
          <span>{banner()}</span>
          <A href="/settings" class="sec-banner-link">
            review
          </A>
        </div>
      </Show>
      <CommandPalette />
      {/* The one step-up dialog (ADR-0073): the API client opens it whenever
          a request is refused with `step_up_required`. */}
      <StepUpDialog />
      {props.children}
    </div>
  );
}
