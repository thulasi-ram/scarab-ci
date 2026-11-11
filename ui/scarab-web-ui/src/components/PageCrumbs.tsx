// The pieces every breadcrumb heading is built from, so each trail starts at
// home: `/ › acme/orders-api › #22`, `/ › Environments › Production`.
import { Show } from "solid-js";
import { A } from "@solidjs/router";
import Icon from "./Icon";
import { orgPageHref } from "../org-page";

/** `/`, a link to the dashboard, and the chevron after it. Every breadcrumb
 *  heading starts with it. */
export function HomeCrumb(props: { size: number; sepClass: string }) {
  return (
    <>
      <A href="/" class="crumb-home" end aria-label="Home" title="Home: the dashboard">
        /
      </A>
      <Icon icon="chevron-right" size={props.size} class={props.sepClass} />
    </>
  );
}

/** `acme/orders-api` as one crumb: the org links to the org page, and the
 *  repository links to its own page when `link` is set (it is the page itself
 *  on the repository screen). */
export function RepoCrumb(props: { org: string; repo: string; link?: boolean }) {
  return (
    <span class="crumb-repo">
      <A
        href={orgPageHref(props.org)}
        class="crumb-head-org"
        title={`${props.org}: repositories and recent runs`}
      >
        {props.org}
      </A>
      <span class="head-org">/</span>
      <Show when={props.link} fallback={<span>{props.repo}</span>}>
        <A href={`/${props.org}/${props.repo}`} class="crumb-head-link">
          {props.repo}
        </A>
      </Show>
    </span>
  );
}
