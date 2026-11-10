// One forge connection's page, `/orgs/<org>/settings/connections/<id>`,
// inside the settings frame with Connections marked in the sidebar.
//
// It holds what the list row summarises and everything done to the
// connection: its id and addresses, whether its credential resolves, its last
// webhook delivery, the app configuration check (GitHub), Re-sync, Manage
// repositories (the org page narrowed to it), Manage on GitHub, and Remove,
// which asks for the id to be typed. A connection managed by configuration is
// read-only here: the `connections:` block is authoritative, and an edit made
// here would be reverted on the next boot.
//
// The server has no `GET /v1/connections/{id}`, so the page reads the org's
// list and finds the connection in it by id. A connection that serves no org
// is not in any org's list and has no page; it is listed on Connections for a
// global admin.
import { createMemo, createResource, createSignal, Show } from "solid-js";
import { A, useNavigate, useParams } from "@solidjs/router";
import { deleteConnection, listConnections, resyncConnection, type Connection } from "../api/client";
import { connectionChips, connectionHost, enabledReposLabel, forgeName, resyncNote } from "../connections";
import { absTime, relTime } from "../fmt";
import { orgPageHref } from "../org-page";
import { decodeParam, orgHref } from "../settings-nav";
import { SettingsCrumbs, SettingsFrame } from "./Settings";
import { Section } from "./LibraryManage";
import Chips from "../components/Chips";
import Icon from "../components/Icon";
import { AppPreflight, CredentialFact, manageUrl, RemoveConnection } from "../components/Connections";

/** `/orgs/:org/settings/connections/:id`. */
export default function ConnectionManage() {
  const params = useParams<{ org: string; id: string }>();
  const org = () => decodeParam(params.org);
  const id = () => decodeParam(params.id);
  const key = createMemo(() => JSON.stringify([org(), id()]));
  return (
    <SettingsFrame kind="org" org={org()} section="connections">
      {/* Keyed: another connection in the URL is a new page. */}
      <Show when={key()} keyed>
        {(_key) => <Manage org={org()} id={id()} />}
      </Show>
    </SettingsFrame>
  );
}

function Manage(props: { org: string; id: string }) {
  const nav = useNavigate();
  const back = () => orgHref(props.org, "connections");
  const [conns, { refetch }] = createResource(() => listConnections(props.org));
  const conn = () => (conns.error ? undefined : conns()?.find((c) => c.id === props.id));

  const [notice, setNotice] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);

  return (
    <>
      <SettingsCrumbs
        trail={[{ label: "Connections", href: back() }, { label: props.id, mono: true }]}
        sub={
          <>
            <span class="mono">{props.org}</span> · a forge connection
          </>
        }
      />
      <Show when={notice()}>
        <p class="notice-ok">{notice()}</p>
      </Show>
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
      <Show when={!conns.error} fallback={<p class="error">Could not load the org's connections.</p>}>
        <Show when={!conns.loading || conn()} fallback={<p class="empty">loading…</p>}>
          <Show
            when={conn()}
            fallback={
              <p class="empty">
                <b class="mono">{props.id}</b> is not a connection of {props.org}.{" "}
                <A href={back()}>All connections</A>
              </p>
            }
          >
            {(c) => (
              <Details
                org={props.org}
                conn={c()}
                onNotice={(msg) => {
                  setError(null);
                  setNotice(msg);
                  void refetch();
                }}
                onError={(msg) => {
                  setNotice(null);
                  setError(msg);
                }}
                onRemoved={() => nav(back())}
              />
            )}
          </Show>
        </Show>
      </Show>
    </>
  );
}

function Details(props: {
  org: string;
  conn: Connection;
  onNotice: (msg: string) => void;
  onError: (msg: string) => void;
  onRemoved: () => void;
}) {
  const c = () => props.conn;
  const [syncing, setSyncing] = createSignal(false);
  const [removing, setRemoving] = createSignal(false);
  const editable = () => !c().managed_by_config;

  const resync = async () => {
    setSyncing(true);
    try {
      props.onNotice(resyncNote(await resyncConnection(c().id)));
    } catch (e) {
      props.onError(e instanceof Error ? e.message : "Re-sync failed.");
    } finally {
      setSyncing(false);
    }
  };

  return (
    <>
      <div class="run-toolbar">
        <A class="btn btn-primary btn-sm" href={orgPageHref(props.org, c().id)}>
          Manage repositories
        </A>
        <Show when={c().supports_resync && editable()}>
          <button
            class="btn btn-ghost btn-sm"
            disabled={syncing()}
            title="Ask the forge which repositories this connection reaches and list the ones not enabled"
            onClick={() => void resync()}
          >
            <Icon icon="rotate-cw" size={13} /> {syncing() ? "Re-syncing…" : "Re-sync"}
          </button>
        </Show>
        <Show when={manageUrl(c())}>
          {(href) => (
            <a class="btn btn-ghost btn-sm" href={href()} target="_blank" rel="noreferrer">
              Manage on {forgeName(c().kind)} →
            </a>
          )}
        </Show>
      </div>

      <Section title="Connection" aside={<Chips chips={connectionChips(c())} />}>
        <dl class="kv">
          <dt>Forge</dt>
          <dd>
            {forgeName(c().kind)} · <span class="mono">{connectionHost(c())}</span>
          </dd>
          <dt>Id</dt>
          <dd class="m">{c().id}</dd>
          <dt>API address</dt>
          <dd class="m">{c().base_url}</dd>
          <dt>Repositories</dt>
          <dd>{enabledReposLabel(c().enabled_repos)}</dd>
          <Show when={c().managed_by_config}>
            <dt>Managed</dt>
            <dd>
              By the server's <span class="mono">connections:</span> configuration (Helm{" "}
              <span class="mono">scarab.connections</span> or <span class="mono">SCARAB_CONNECTIONS</span>).
              Change or remove it there.
            </dd>
          </Show>
        </dl>
      </Section>

      <Section title="Credential">
        <dl class="kv">
          <dt>Handle</dt>
          <dd class="m">{c().credential_ref}</dd>
          <dt>Status</dt>
          <dd>
            <CredentialFact conn={c()} />
          </dd>
        </dl>
        <p class="kv-note">The token is stored encrypted and is never shown.</p>
      </Section>

      <Section title="Webhook deliveries">
        <dl class="kv">
          <dt>Last delivery</dt>
          <dd title={c().last_delivery_at != null ? absTime(c().last_delivery_at!) : undefined}>
            {c().last_delivery_at != null ? relTime(c().last_delivery_at!) : "None recorded yet."}
          </dd>
        </dl>
        <p class="kv-note">
          Deliveries are recorded per kind of forge, so with two {forgeName(c().kind)} connections this is the
          latest delivery to either.
        </p>
      </Section>

      <Show when={c().supports_preflight}>
        <Section title="App configuration">
          <div class="secrets-body">
            <AppPreflight conn={c()} />
          </div>
        </Section>
      </Show>

      <Show when={editable()}>
        <Section
          title="Remove"
          danger
          foot={
            <button class="btn btn-danger btn-sm" onClick={() => setRemoving(true)}>
              Remove connection
            </button>
          }
        >
          <p class="kv-note kv-lead">
            Removing it also removes the projects it serves, with their environments, secrets and
            access grants.
          </p>
        </Section>
      </Show>

      <Show when={removing()}>
        <RemoveConnection
          conn={c()}
          onConfirm={async () => {
            await deleteConnection(c().id, c().enabled_repos > 0);
            props.onRemoved();
          }}
          onClose={() => setRemoving(false)}
        />
      </Show>
    </>
  );
}
