// Settings (git-bug 734ffa1): one shell, an in-page sidebar, one URL per
// section.
//
// The sidebar sits inside the usual 1200px `.page` column, not beside the
// app: the top bar stays the only app-wide navigation. Two separate pages,
// each listing only its own sections: Account settings (`/settings/<section>`,
// everyone's) and Organization settings (`/orgs/<org>/settings/<section>`,
// admins of that org, with an org dropdown at the top of the sidebar). The
// user menu links each one. The org dropdown's last entry, for global admins,
// is New organization… (`/orgs/_new`): a new org's first forge connection, in
// the same frame, and the connections that serve no org yet, which the entry
// counts. Under ~900px the section list becomes a `<select>` above
// the section, with the org dropdown above it. A page under a section (one
// Environment, a library review) renders in the same frame, with its parent
// section marked. The route table, the gating, which section a path sits in
// and the old-URL redirects are pure functions in `settings-nav.ts`.
//
// Gating here is presentation: the server enforces `Administer` on the org
// for every request an org section makes.
import { For, Match, Show, Switch, createMemo, type JSX } from "solid-js";
import { A, Navigate, useLocation, useNavigate, useParams } from "@solidjs/router";
import { me } from "../session";
import {
  activeSectionHref,
  administers,
  legacyRedirect,
  navGroup,
  decodeParam,
  groupHome,
  NEW_ORG_CHOICE,
  NEW_ORG_LABEL,
  newOrgEntryLabel,
  orgPickHref,
  resolveSection,
  settingsHome,
  showsOrgPick,
  type OrgChoice,
  type SettingsGroupKind,
  type SettingsSection,
} from "../settings-nav";
import LinkedAccounts from "../components/LinkedAccounts";
import SecuritySettings from "../components/SecuritySettings";
import AppearanceSettings from "../components/AppearanceSettings";
import Connections, { NewOrgConnection, WaitingConnections } from "../components/Connections";
import { waitingCount } from "../waiting-connections";
import Members from "../components/Members";
import OrgEnvironments from "../components/OrgEnvironments";
import Plugins from "../components/Plugins";
import Libraries from "../components/Libraries";
import ScopedSecrets from "../components/ScopedSecrets";
import { MyApiTokens, OrgApiTokens } from "../components/ApiTokens";
import OrgSecurity from "../components/OrgSecurity";
import OrgPolicies from "../components/OrgPolicies";
import PolicyLists from "../components/PolicyLists";
import {
  LIBRARY_KINDS,
  LIBRARY_POLICIES_ANCHOR,
  OLD_LIBRARY_POLICIES_ANCHOR,
  OLD_PLUGIN_POLICIES_ANCHOR,
  PLUGIN_KINDS,
  PLUGIN_POLICIES_ANCHOR,
} from "../policies";
import Icon from "../components/Icon";

const IDENTITY_ERROR = "Could not load your identity. Is the server up?";

/** `/settings/:section` — a section of Account settings. */
export function AccountSettings() {
  const params = useParams<{ section: string }>();
  return <SettingsShell kind="account" section={params.section} />;
}

/** `/orgs/:org/settings/:section?` — a section of one org's settings. */
export function OrgSettings() {
  const params = useParams<{ org: string; section?: string }>();
  return <SettingsShell kind="org" org={decodeParam(params.org)} section={params.section} />;
}

/** `/orgs/_new` — a new org, made by its first forge connection (global
 *  admins), under the connections that wait for an org, which can make one
 *  too. It renders in the org settings frame, with New organization… picked
 *  in the org dropdown. */
export function NewOrgSettings() {
  return (
    <SettingsFrame kind="org">
      <div class="page-head">
        <h1>{NEW_ORG_LABEL}</h1>
      </div>
      <p class="page-sub">its first forge connection</p>
      <WaitingConnections />
      <NewOrgConnection />
    </SettingsFrame>
  );
}

/** An old settings URL (`/settings`, `/account`, `/settings/server/…`, an
 *  org's former Event log, …): where it lives now. */
export function LegacySettings() {
  const location = useLocation();
  return (
    <Show
      when={me()}
      fallback={
        <Show when={me.error} fallback={<p class="empty page">loading…</p>}>
          <p class="error page">{IDENTITY_ERROR}</p>
        </Show>
      }
    >
      {(who) => (
        <Navigate
          href={legacyRedirect(location.pathname, location.search, who()) ?? settingsHome(who())}
        />
      )}
    </Show>
  );
}

function SettingsShell(props: { kind: SettingsGroupKind; org?: string; section?: string }) {
  const section = () => resolveSection(props.kind, props.section);
  return (
    // `/orgs/<org>/settings` alone opens the org's first section.
    <Show when={props.section} fallback={<Navigate href={groupHome(props.kind, props.org)} />}>
      <SettingsFrame kind={props.kind} org={props.org} section={props.section}>
        <Show when={section()} fallback={<NotFound />}>
          {(s) => <SectionPage kind={props.kind} org={props.org} section={s()} />}
        </Show>
      </SettingsFrame>
    </Show>
  );
}

/**
 * The settings page around `children`: the sidebar, the narrow-screen
 * `<select>`, and the main column. A section renders in it, and so does a
 * page under a section (one Environment, a library review), so the sidebar
 * stays in view and marks the section the page belongs to.
 *
 * On an org page the main column shows `children` only to an admin of that
 * org, and on the New organization page (`kind` org, no `org`) only to a
 * global admin; everyone else sees why not.
 */
export function SettingsFrame(props: {
  kind: SettingsGroupKind;
  /** The org in the URL; on an org page, undefined means New organization. */
  org?: string;
  /** The section id, so switching orgs lands on the same section. */
  section?: string;
  children: JSX.Element;
}) {
  const nav = useNavigate();
  const location = useLocation();
  const group = createMemo(() => (me() ? navGroup(me()!, props.kind, props.org) : undefined));
  const active = createMemo(() => activeSectionHref(location.pathname));

  return (
    <section class="page">
      <Show when={me.error}>
        <p class="error">{IDENTITY_ERROR}</p>
      </Show>
      <Show when={group()} fallback={<Show when={!me.error}><p class="empty">loading…</p></Show>}>
        {(g) => (
          <div class="settings">
            <nav class="settings-nav" aria-label="Settings">
              <div class="settings-nav-group">
                <Show when={showsOrgPick(g())}>
                  <OrgPick org={g().org} orgs={g().orgs ?? []} newOrg={!!g().newOrg} section={props.section} />
                </Show>
                <div class="settings-nav-h">{g().heading}</div>
                <For each={g().items}>
                  {(item) => (
                    <A
                      href={item.href}
                      class="settings-nav-link"
                      classList={{ on: active() === item.href }}
                      aria-current={active() === item.href ? "page" : undefined}
                    >
                      {item.label}
                    </A>
                  )}
                </For>
              </div>
            </nav>

            <div class="settings-main">
              <Show when={g().items.length > 0}>
                <select
                  class="input settings-select"
                  aria-label="Settings section"
                  onChange={(e) => nav(e.currentTarget.value)}
                >
                  <For each={g().items}>
                    {(item) => (
                      <option value={item.href} selected={active() === item.href}>
                        {item.label}
                      </option>
                    )}
                  </For>
                </select>
              </Show>

              <Switch fallback={props.children}>
                <Match when={props.kind === "org" && props.org === undefined && !me()?.global_admin}>
                  <NotGlobalAdmin />
                </Match>
                <Match when={props.kind === "org" && props.org !== undefined && !administers(me(), props.org)}>
                  <NotYours org={props.org!} />
                </Match>
              </Switch>
            </div>
          </div>
        )}
      </Show>
    </section>
  );
}

/** One step of a page's trail; the last is the page itself (no `href`). */
export interface Crumb {
  label: string;
  href?: string;
  mono?: boolean;
}

/**
 * The heading of a page under a section, inside the settings main column:
 * the trail from the section down to this page, e.g.
 * Environments › production › Edit.
 */
export function SettingsCrumbs(props: { trail: Crumb[]; sub: JSX.Element }) {
  return (
    <>
      <div class="page-head">
        <h1 class="settings-crumb">
          <For each={props.trail}>
            {(c, i) => (
              <>
                <Show when={i() > 0}>
                  <Icon icon="chevron-right" size={16} class="settings-crumb-sep" />
                </Show>
                <Show
                  when={c.href}
                  fallback={
                    <span class="settings-crumb-here" classList={{ mono: !!c.mono }} aria-current="page">
                      {c.label}
                    </span>
                  }
                >
                  {(href) => (
                    <A href={href()} class="settings-crumb-link" classList={{ mono: !!c.mono }}>
                      {c.label}
                    </A>
                  )}
                </Show>
              </>
            )}
          </For>
        </h1>
      </div>
      <p class="page-sub">{props.sub}</p>
    </>
  );
}

/**
 * The org dropdown at the top of an org page's sidebar, shown even for one
 * org so it is always in the same place. Picking an org keeps the section in
 * view. For a global admin its last entry is New organization…, picked on
 * that page (`org` undefined), with the count of connections that wait for an
 * org there. Under ~900px it stays above the section `<select>`.
 */
function OrgPick(props: { org?: string; orgs: OrgChoice[]; newOrg: boolean; section?: string }) {
  const nav = useNavigate();
  return (
    <select
      class="input settings-nav-orgpick mono"
      aria-label="Organization"
      onChange={(e) => nav(orgPickHref(e.currentTarget.value, props.section))}
    >
      <For each={props.orgs}>
        {(c) => (
          <option value={c.org} selected={c.org === props.org} disabled={!c.administered}>
            {c.org}
          </option>
        )}
      </For>
      <Show when={props.newOrg}>
        <option value={NEW_ORG_CHOICE} selected={props.org === undefined}>
          {newOrgEntryLabel(waitingCount(), true)}
        </option>
      </Show>
    </select>
  );
}

function SectionPage(props: { kind: SettingsGroupKind; org?: string; section: SettingsSection }) {
  // The body is built once per org and section: a body reads the org when it
  // is built, so switching org on the same section (the org picker, ⌘K) must
  // build it again, not only switching section. The child keeps its
  // parameter: Show calls a child function only when it takes one.
  const key = createMemo(() => `${props.kind}:${props.org ?? ""}:${props.section.id}`);
  return (
    <>
      <div class="page-head">
        <h1>{props.section.label}</h1>
      </div>
      <p class="page-sub">
        <Show when={props.kind === "org"} fallback="your account">
          <span class="mono">{props.org}</span>
        </Show>{" "}
        · {props.section.sub}
      </p>
      <Show when={key()} keyed>
        {(_key) =>
          props.kind === "account" ? accountBody(props.section.id) : orgBody(props.section.id, props.org!)
        }
      </Show>
    </>
  );
}

function accountBody(id: string): JSX.Element {
  switch (id) {
    case "profile":
      return <LinkedAccounts />;
    case "security":
      return <SecuritySettings />;
    case "tokens":
      return <MyApiTokens />;
    case "appearance":
      return <AppearanceSettings />;
    default:
      return <NotFound />;
  }
}

function orgBody(id: string, org: string): JSX.Element {
  switch (id) {
    case "connections":
      return <Connections org={org} />;
    case "members":
      return <Members org={org} />;
    case "environments":
      return <OrgEnvironments org={org} />;
    case "plugins":
      return (
        <>
          <Plugins org={org} />
          <PolicyLists
            org={org}
            kinds={PLUGIN_KINDS}
            anchor={PLUGIN_POLICIES_ANCHOR}
            oldAnchor={OLD_PLUGIN_POLICIES_ANCHOR}
            title="Plugin policies"
          />
        </>
      );
    case "libraries":
      return (
        <>
          <Libraries org={org} />
          <PolicyLists
            org={org}
            kinds={LIBRARY_KINDS}
            anchor={LIBRARY_POLICIES_ANCHOR}
            oldAnchor={OLD_LIBRARY_POLICIES_ANCHOR}
            title="Library policies"
          />
        </>
      );
    case "secrets":
      return (
        <>
          <ScopedSecrets scope={{ org }} title={`Org secrets · ${org}`} emptyLabel="No org-wide secrets yet." />
          <p class="subtle">
            <small>
              <Icon icon="key-round" size={12} /> Org secrets are the base of the inheritance chain —
              every repo and environment in <b>{org}</b> resolves them unless it sets the same key at
              a narrower scope. Per-repo and per-environment values live on the repository's Secrets
              tab.
            </small>
          </p>
        </>
      );
    case "policies":
      return <OrgPolicies org={org} />;
    case "tokens":
      return <OrgApiTokens org={org} />;
    case "security":
      return <OrgSecurity org={org} />;
    default:
      return <NotFound />;
  }
}

function NotYours(props: { org: string }) {
  return (
    <div class="panel">
      <div class="panel-h"><span>Organization settings</span></div>
      <div class="secrets-body">
        <p class="empty">
          You do not administer <b class="mono">{props.org}</b>. These settings need the{" "}
          <b>Administer</b> capability on the organization; ask an owner if you need access.
        </p>
      </div>
    </div>
  );
}

function NotGlobalAdmin() {
  return (
    <div class="panel">
      <div class="panel-h"><span>{NEW_ORG_LABEL}</span></div>
      <div class="secrets-body">
        <p class="empty">
          A new organization needs a global <b>Administer</b> role. Ask a server administrator if
          you need one connected.
        </p>
      </div>
    </div>
  );
}

function NotFound() {
  return <p class="empty">There is no such settings section.</p>;
}
