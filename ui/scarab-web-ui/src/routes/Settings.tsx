// Settings (git-bug 734ffa1): one shell, an in-page sidebar, one URL per
// section.
//
// The sidebar sits inside the usual 1200px `.page` column, not beside the
// app: the top bar stays the only app-wide navigation. Two groups — Your
// account (`/settings/<section>`, everyone's) and Organization settings
// (`/orgs/<org>/settings/<section>`, admins of that org). Under ~900px the
// list becomes a `<select>` above the section. The route table, the gating
// and the old-URL redirects are pure functions in `settings-nav.ts`.
//
// Gating here is presentation: the server enforces `Administer` on the org
// for every request an org section makes.
import { For, Match, Show, Switch, createMemo, type JSX } from "solid-js";
import { A, Navigate, useLocation, useNavigate, useParams } from "@solidjs/router";
import { me } from "../session";
import {
  administers,
  legacyRedirect,
  navGroups,
  orgHref,
  resolveSection,
  settingsHome,
  type SettingsGroupKind,
  type SettingsSection,
} from "../settings-nav";
import LinkedAccounts from "../components/LinkedAccounts";
import SecuritySettings from "../components/SecuritySettings";
import Connections from "../components/Connections";
import Members from "../components/Members";
import OrgEnvironments from "../components/OrgEnvironments";
import Plugins from "../components/Plugins";
import Libraries from "../components/Libraries";
import ScopedSecrets from "../components/ScopedSecrets";
import ApiTokens from "../components/ApiTokens";
import OrgSecurity from "../components/OrgSecurity";
import EventLog from "../components/EventLog";
import Icon from "../components/Icon";

/** `/settings/:section` — a section of Your account. */
export function AccountSettings() {
  const params = useParams<{ section: string }>();
  return <SettingsShell kind="account" section={params.section} />;
}

/** `/orgs/:org/settings/:section?` — a section of one org's settings. */
export function OrgSettings() {
  const params = useParams<{ org: string; section?: string }>();
  return <SettingsShell kind="org" org={params.org} section={params.section} />;
}

/** An old settings URL (`/settings`, `/account`, …): where it lives now. */
export function LegacySettings() {
  const location = useLocation();
  return (
    <Show when={me()} fallback={<p class="empty page">loading…</p>}>
      {(who) => (
        <Navigate
          href={legacyRedirect(location.pathname, location.search, who()) ?? settingsHome(who())}
        />
      )}
    </Show>
  );
}

function SettingsShell(props: { kind: SettingsGroupKind; org?: string; section?: string }) {
  const nav = useNavigate();
  const location = useLocation();
  const groups = createMemo(() => (me() ? navGroups(me()!, props.org) : []));
  const section = () => resolveSection(props.kind, props.section);

  return (
    <section class="page">
      <Show when={me.error}>
        <p class="error">Could not load your identity. Is the server up?</p>
      </Show>
      <Show when={me()} fallback={<Show when={!me.error}><p class="empty">loading…</p></Show>}>
        {/* `/orgs/<org>/settings` alone opens the org's first section. */}
        <Show when={props.kind !== "org" || props.section}>
          <div class="settings">
            <nav class="settings-nav" aria-label="Settings">
              <For each={groups()}>
                {(g) => (
                  <div class="settings-nav-group">
                    <div class="settings-nav-h">{g.heading}</div>
                    <Show when={g.org}>
                      {(o) => <OrgSwitch org={o()} others={g.otherOrgs ?? []} section={props.section} />}
                    </Show>
                    <For each={g.items}>
                      {(item) => (
                        <A
                          href={item.href}
                          class="settings-nav-link"
                          classList={{ on: location.pathname === item.href }}
                          aria-current={location.pathname === item.href ? "page" : undefined}
                        >
                          {item.label}
                        </A>
                      )}
                    </For>
                  </div>
                )}
              </For>
            </nav>

            <div class="settings-main">
              <select
                class="input settings-select"
                aria-label="Settings section"
                onChange={(e) => nav(e.currentTarget.value)}
              >
                <For each={groups()}>
                  {(g) => (
                    <optgroup label={g.org ? `${g.heading} · ${g.org}` : g.heading}>
                      <For each={g.items}>
                        {(item) => (
                          <option value={item.href} selected={location.pathname === item.href}>
                            {item.label}
                          </option>
                        )}
                      </For>
                    </optgroup>
                  )}
                </For>
              </select>

              <Switch fallback={<NotFound />}>
                <Match when={props.kind === "org" && !administers(me(), props.org)}>
                  <NotYours org={props.org!} />
                </Match>
                <Match when={section()}>
                  {(s) => <SectionPage kind={props.kind} org={props.org} section={s()} />}
                </Match>
              </Switch>
            </div>
          </div>
        </Show>
        <Show when={props.kind === "org" && !props.section}>
          <Navigate href={orgHref(props.org!, "connections")} />
        </Show>
      </Show>
    </section>
  );
}

/** The org name in the group heading; a picker when there is more than one. */
function OrgSwitch(props: { org: string; others: string[]; section?: string }) {
  const nav = useNavigate();
  return (
    <Show when={props.others.length > 0} fallback={<div class="settings-nav-org mono">{props.org}</div>}>
      <select
        class="input settings-nav-orgpick mono"
        aria-label="Organization"
        onChange={(e) => nav(orgHref(e.currentTarget.value, props.section ?? "connections"))}
      >
        <For each={[props.org, ...props.others].sort()}>
          {(o) => (
            <option value={o} selected={o === props.org}>
              {o}
            </option>
          )}
        </For>
      </select>
    </Show>
  );
}

function SectionPage(props: { kind: SettingsGroupKind; org?: string; section: SettingsSection }) {
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
      <Show when={props.section.id} keyed>
        {(id) => (props.kind === "account" ? accountBody(id) : orgBody(id, props.org!))}
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
    default:
      return <NotFound />;
  }
}

function orgBody(id: string, org: string): JSX.Element {
  switch (id) {
    case "connections":
      return <Connections />;
    case "members":
      return <Members org={org} />;
    case "environments":
      return <OrgEnvironments org={org} />;
    case "plugins":
      return <Plugins org={org} />;
    case "libraries":
      return <Libraries org={org} />;
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
    case "tokens":
      return <ApiTokens org={org} />;
    case "security":
      return <OrgSecurity org={org} />;
    case "events":
      return <EventLog org={org} />;
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

function NotFound() {
  return <p class="empty">There is no such settings section.</p>;
}
