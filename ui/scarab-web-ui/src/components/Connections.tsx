// Forge connections: the Settings section that answers "is my forge still
// wired up, and what does it cover?"
//
// A connection is the thing that makes a repo a Project: `ForgeConnection`
// resolves a forge coordinate to a governed Project, and a Project *is* that
// binding. So this list is the whole footprint of Scarab's reach.
//
// The section lists one row per connection: its forge and host, linking to
// the connection's own page (`…/settings/connections/<id>`,
// routes/ConnectionManage.tsx), its state tags, how many repositories it has
// enabled and its last delivery. Everything done to one connection (re-sync,
// the app's configuration check, removing it) is on that page.
//
// The connection itself differs by forge:
//
// - **GitHub** connections register themselves: installing the App creates
//   the connection. Scarab cannot install, uninstall or change which repos it
//   covers, so the connection's page links to GitHub for that.
// - **Forgejo** emits no installation event, so an admin adds the connection
//   here (base URL + token). This is the one place a Forgejo connection is
//   added: Add Forgejo connection, top right of an org's Connections, or, for
//   an org that does not exist yet, the New organization page
//   (`NewOrgConnection`), which shows the same form with a field for the
//   org's name.
//
// Repositories are opt-in for both, and they are enabled and disabled in one
// place: the org page (`/orgs/<org>`), which pages and filters them. A
// connection's page links to the org page narrowed to it (Manage
// repositories); nothing here lists them, since a connection can reach
// thousands.
//
// The token is write-only: it is typed once, posted once, and never rendered.
// The page reports whether the *handle* resolves, which is the question an
// operator has ("is my forge still wired up?").
//
// Each connection serves one org (git-bug 19b0a45): the forge account that owns
// the repositories it binds. This section lists the connections of the org in
// the URL. A connection that serves no org yet ("unassigned") is listed to a
// global admin in a panel of its own, on every org's page, because no org's
// list includes it; it has no page of its own, since no org's list finds it.
import { createResource, createSignal, For, Show, type JSX } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import {
  assignConnectionOrg,
  connectionPreflight,
  createConnection,
  deleteConnection,
  listConnections,
  type CapabilityRequirement,
  type Connection,
} from "../api/client";
import {
  connectionChips,
  connectionHost,
  connectionHref,
  enabledReposLabel,
  forgeName,
  orgNameProblem,
  unassignedConnections,
} from "../connections";
import { absTime, relTime } from "../fmt";
import { me, refetchMe } from "../session";
import { FIRST_ORG_SECTION, orgHref } from "../settings-nav";
import Chips from "./Chips";
import FormSection from "./FormSection";
import Modal, { ModalMessage, ModalSection } from "./Modal";
import ConfirmDestructive from "./ConfirmDestructive";
import { Field, Input } from "./Field";
import Icon from "./Icon";

/** Where a connection's own management lives, when the forge has such a page. */
export function manageUrl(c: Connection): string | null {
  // GitHub App installations are managed per-account on github.com; the exact
  // installation id isn't in our model, so link to the account's app settings
  // where the install is listed.
  return c.kind === "github" ? `${c.web_url}/settings/installations` : null;
}

export default function Connections(props: { org: string }) {
  const [epoch, setEpoch] = createSignal(0);
  const [conns] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listConnections(k.org),
  );
  // Only a global admin is sent unassigned connections; for anyone else the
  // request is not made.
  const global = () => me()?.global_admin === true;
  const [unassigned] = createResource(
    () => (global() ? { epoch: epoch() } : false),
    () => listConnections().then(unassignedConnections),
  );
  const [adding, setAdding] = createSignal(false);
  const reload = () => setEpoch((e) => e + 1);
  const rows = () => (conns.error ? [] : (conns() ?? []));

  return (
    <>
      <div class="panel">
        <div class="panel-h">
          <span>
            Forge connections · {props.org} <span class="tok-count">{rows().length}</span>
          </span>
          <button class="btn btn-primary btn-sm" onClick={() => setAdding(true)}>
            <Icon icon="plus" size={14} /> Add Forgejo connection
          </button>
        </div>
        <div class="secrets-body">
          <Show when={conns.error}>
            <p class="error">Could not load connections.</p>
          </Show>
          <Show when={!conns.loading} fallback={<p class="empty">loading…</p>}>
            <Show
              when={rows().length > 0}
              fallback={
                <Show when={!conns.error}>
                  <p class="empty">
                    No forge connected to <b class="mono">{props.org}</b> yet.
                  </p>
                </Show>
              }
            >
              <table class="table lib-table conn-table" aria-label="Forge connections">
                <thead>
                  <tr>
                    <th>Connection</th>
                    <th>State</th>
                    <th>Repositories</th>
                    <th>Last delivery</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  <For each={rows()}>
                    {(c) => (
                      <tr>
                        <td>
                          <A class="oenv-link" href={connectionHref(props.org, c.id)}>
                            {forgeName(c.kind)} <span class="mono">{connectionHost(c)}</span>
                          </A>
                          <span class="conn-id mono">{c.id}</span>
                        </td>
                        <td class="keep">
                          <Chips chips={connectionChips(c)} />
                        </td>
                        <td>{enabledReposLabel(c.enabled_repos)}</td>
                        <td title={c.last_delivery_at != null ? absTime(c.last_delivery_at) : undefined}>
                          {c.last_delivery_at != null ? relTime(c.last_delivery_at) : "none yet"}
                        </td>
                        <td class="acts keep">
                          <A class="btn btn-ghost btn-sm" href={connectionHref(props.org, c.id)}>
                            Manage
                          </A>
                        </td>
                      </tr>
                    )}
                  </For>
                </tbody>
              </table>
            </Show>
            <p class="subtle">
              <small>
                GitHub connections register themselves when the Scarab App is installed on the{" "}
                <span class="mono">{props.org}</span> account. For Forgejo or Codeberg, use Add
                Forgejo connection. A connection binds only repositories owned by{" "}
                <span class="mono">{props.org}</span>, and only the ones you enable from its page.
              </small>
            </p>
          </Show>
        </div>
        <Show when={adding()}>
          <ForgejoConnectionForm
            org={props.org}
            onSaved={() => {
              reload();
              setAdding(false);
            }}
            onCancel={() => setAdding(false)}
          />
        </Show>
      </div>

      <Show when={global() && (unassigned()?.length ?? 0) > 0}>
        <div class="panel">
          <div class="panel-h"><span>Unassigned connections</span></div>
          <div class="secrets-body">
            <p class="subtle">
              <small>
                These connections serve no organization, so only global admins see them, here
                on every organization's page. Assign one to the organization that owns its
                repositories. If its repositories belong to more than one organization, give each
                organization its own connection and re-sync it there: the repositories move to it
                with their projects. Then remove this one.
              </small>
            </p>
            <ul class="conn-list">
              <For each={unassigned()}>
                {(c) => <UnassignedRow conn={c} onChanged={reload} />}
              </For>
            </ul>
          </div>
        </div>
      </Show>
    </>
  );
}

/**
 * The New organization page's body: a new org's first connection, for a
 * global admin. An org comes into being with its first connection, so this
 * is the connection form without a fixed org: the admin names it. On success
 * it lands on the new org's Connections, where any further connection is
 * added.
 */
export function NewOrgConnection() {
  const nav = useNavigate();
  return (
    <div class="panel">
      <div class="panel-h"><span>First forge connection</span></div>
      <div class="secrets-body">
        <p class="subtle">
          An organization in Scarab is a forge account: the user or organization that owns the
          repositories. It is created by its first connection.
        </p>
        <p class="subtle">
          <small>
            For GitHub, install the Scarab App on the account instead. The installation registers
            the connection and the organization.
          </small>
        </p>
        <ForgejoConnectionForm
          onSaved={async (org) => {
            // The new org is in `admin_orgs` only once `/v1/me` is asked again.
            // Then its home section, which is its Connections.
            await refetchMe();
            nav(orgHref(org, FIRST_ORG_SECTION));
          }}
        />
      </div>
    </div>
  );
}

/**
 * Add a Forgejo/Codeberg connection: the org it serves, an instance URL and an
 * access token.
 *
 * Forgejo-only by construction — a GitHub form would be a lie (the server
 * refuses it, because installing the App is the registration). The token field is
 * `type="password"` and write-only end to end: it is posted once and the server
 * stores it under a handle it generates itself, so there is no code path, here or
 * on the server, that can render it back.
 *
 * With `org`, the org is fixed (the org page it was opened from) and shown
 * read-only; without it, the caller names a new org (the New organization
 * page). With `onCancel` it is a dialog: the same sections in a Modal, the
 * actions in its footer.
 */
function ForgejoConnectionForm(props: {
  org?: string;
  onSaved: (org: string) => void | Promise<void>;
  onCancel?: () => void;
}) {
  const [newOrg, setNewOrg] = createSignal("");
  const [baseUrl, setBaseUrl] = createSignal("");
  const [token, setToken] = createSignal("");
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const org = () => props.org ?? newOrg().trim();
  const orgProblem = () => (props.org ? null : orgNameProblem(newOrg()));
  const ready = () =>
    orgProblem() == null && baseUrl().trim().length > 0 && token().trim().length > 0;

  const save = async () => {
    if (!ready() || saving()) return;
    setSaving(true);
    setError(null);
    try {
      await createConnection({
        kind: "forgejo",
        org: org(),
        base_url: baseUrl().trim(),
        credential: token().trim(),
      });
      await props.onSaved(org());
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not create the connection.");
    } finally {
      setSaving(false);
    }
  };

  // A dialog's sections are ModalSections; on a page they are FormSections.
  const Sec = (p: { n: number; title: string; children: JSX.Element }) =>
    props.onCancel ? <ModalSection {...p} /> : <FormSection {...p} />;

  const sections = (
    <>
      <Sec n={1} title="Organization">
        <Show
          when={props.org}
          fallback={
            <Field
              id="conn-org"
              label="Forge account that owns the repositories"
              required
              error={newOrg().trim() !== "" ? orgProblem() : null}
            >
              <Input mono placeholder="acme" value={newOrg()} onValue={setNewOrg} />
            </Field>
          }
        >
          {(o) => (
            <Field label="Organization" help="The connection binds only repositories owned by this account.">
              <Input mono readOnly value={o()} onValue={() => {}} />
            </Field>
          )}
        </Show>
      </Sec>
      <Sec n={2} title="Forgejo instance">
        <Field id="conn-url" label="Instance URL" required>
          <Input mono placeholder="https://codeberg.org" value={baseUrl()} onValue={setBaseUrl} />
        </Field>
      </Sec>
      <Sec n={3} title="Access token">
        <Field
          id="conn-token"
          label="Token"
          required
          help="It needs repository read and webhook administration on the repositories you intend to build. It is stored encrypted and never displayed again."
        >
          <Input type="password" placeholder="token (write-only)" value={token()} onValue={setToken} />
        </Field>
      </Sec>
    </>
  );
  const add = (
    <button class="btn btn-primary" type="button" disabled={saving() || !ready()} onClick={save}>
      <Icon icon="plus" size={14} /> {saving() ? "Connecting…" : "Add connection"}
    </button>
  );

  return (
    <Show
      when={props.onCancel}
      fallback={
        <div class="conn-form">
          {sections}
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">{add}</div>
        </div>
      }
    >
      {(cancel) => (
        <Modal
          eyebrow={`${props.org} · connections`}
          title="Add Forgejo connection"
          purpose="Connect a Forgejo or Codeberg instance. Its repositories become projects once you enable them on the organization page."
          busy={saving()}
          onClose={() => cancel()()}
          footer={
            <>
              <Show when={error()}>
                <ModalMessage>{error()}</ModalMessage>
              </Show>
              <button class="btn btn-ghost" type="button" onClick={() => cancel()()} disabled={saving()}>
                Cancel
              </button>
              {add}
            </>
          }
        >
          {sections}
        </Modal>
      )}
    </Show>
  );
}

/**
 * A connection that serves no org, for a global admin: its forge and id, its
 * credential and last delivery, Assign to org and Remove. It has no page of
 * its own and offers no re-sync, since it binds nothing for an org.
 */
function UnassignedRow(props: { conn: Connection; onChanged: () => void }) {
  const c = () => props.conn;
  const [note, setNote] = createSignal<string | null>(null);
  const [removing, setRemoving] = createSignal(false);
  const [assigning, setAssigning] = createSignal(false);

  // Removing a connection removes the Projects it serves, so the server
  // refuses unless that is acknowledged; a refusal shows in the confirm.
  const remove = async () => {
    await deleteConnection(c().id, c().enabled_repos > 0);
    props.onChanged();
  };

  return (
    <li class="conn">
      <div class="conn-head">
        <span class="conn-kind mono">{c().kind}</span>
        <code class="mono conn-base">{c().base_url}</code>
        <Show when={c().managed_by_config}>
          <span
            class="conn-tag"
            title="Provisioned from the server's connections: configuration (Helm scarab.connections / SCARAB_CONNECTIONS) and authoritative there. It cannot be changed or removed here."
          >
            managed by configuration
          </span>
        </Show>
        <span class="topbar-spacer" />
        <Show when={manageUrl(c())}>
          {(href) => (
            <a class="btn btn-ghost btn-sm" href={href()} target="_blank" rel="noreferrer">
              Manage on {forgeName(c().kind)} →
            </a>
          )}
        </Show>
        <Show when={!c().managed_by_config}>
          <button
            class="btn btn-ghost btn-sm"
            disabled={assigning()}
            title="Choose the organization this connection serves"
            onClick={() => setAssigning(true)}
          >
            Assign to org
          </button>
          <button
            class="btn btn-danger btn-sm"
            disabled={removing()}
            title="Remove this connection from Scarab"
            onClick={() => setRemoving(true)}
          >
            Remove
          </button>
        </Show>
      </div>

      <div class="conn-facts">
        <span class="subtle">
          id <code class="mono">{c().id}</code>
        </span>
        <CredentialFact conn={c()} />
        <span class="subtle">
          last delivery{" "}
          {c().last_delivery_at != null ? relTime(c().last_delivery_at!) : "none recorded yet"}
        </span>
      </div>

      <Show when={assigning()}>
        <AssignConnection
          conn={c()}
          onAssigned={(org) => {
            setAssigning(false);
            setNote(`Assigned to ${org}. It is listed on that organization's Connections page.`);
            props.onChanged();
          }}
          onCancel={() => setAssigning(false)}
        />
      </Show>

      <Show when={removing()}>
        <RemoveConnection conn={c()} onConfirm={remove} onClose={() => setRemoving(false)} />
      </Show>

      <Show when={note()}>
        <p class="subtle"><small>{note()}</small></p>
      </Show>
    </li>
  );
}

/** Whether a connection's credential handle resolves. The most common silent
 *  breakage is a database restored without its secrets, which no one sees
 *  until a run fails. */
export function CredentialFact(props: { conn: Connection }) {
  const c = () => props.conn;
  return (
    <span classList={{ "conn-ok": c().credential_present, "conn-bad": !c().credential_present }}>
      <Icon icon={c().credential_present ? "shield-check" : "alert-triangle"} size={12} />{" "}
      credential <code class="mono">{c().credential_ref}</code>{" "}
      {c().credential_present ? "resolves" : "is missing: Scarab cannot reach the forge"}
    </span>
  );
}

/**
 * The confirm for removing a connection: what goes with it, and its id typed
 * exactly. Removing it removes every Project it serves.
 */
export function RemoveConnection(props: {
  conn: Connection;
  onConfirm: () => Promise<void>;
  onClose: () => void;
}) {
  const n = () => props.conn.enabled_repos;
  return (
    <ConfirmDestructive
      title={`Remove the ${props.conn.kind} connection?`}
      purpose={<span class="mono">{props.conn.base_url}</span>}
      name={props.conn.id}
      confirmLabel="Remove connection"
      onConfirm={props.onConfirm}
      onClose={props.onClose}
    >
      <p>
        <Show when={n() > 0} fallback="It serves no projects.">
          This also removes its {n()} {n() === 1 ? "project" : "projects"}, with their
          environments, secrets and access grants.
        </Show>
      </p>
    </ConfirmDestructive>
  );
}

/**
 * Assign an unassigned connection to an org, for a global admin. The org is
 * the account that owns the repositories bound through it. The server
 * refuses an org that does not own every bound repository, or one served by
 * another forge, and says why; the reason is shown as it comes.
 */
function AssignConnection(props: {
  conn: Connection;
  onAssigned: (org: string) => void;
  onCancel: () => void;
}) {
  const [org, setOrg] = createSignal("");
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const problem = () => orgNameProblem(org());
  const save = async () => {
    if (problem() != null || saving()) return;
    setSaving(true);
    setError(null);
    try {
      const name = org().trim();
      await assignConnectionOrg(props.conn.id, name);
      props.onAssigned(name);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not assign the connection.");
    } finally {
      setSaving(false);
    }
  };
  return (
    <div class="typed-confirm">
      <p>
        Which organization does <span class="mono">{props.conn.id}</span> serve? Its admins then
        manage it, and it binds only repositories that organization owns.
      </p>
      <Field label="Organization" error={org().trim() !== "" ? problem() : null}>
        <Input
          mono
          aria-label="Organization to assign"
          placeholder="acme"
          value={org()}
          onValue={setOrg}
          onKeyDown={(e) => {
            if (e.key === "Enter") void save();
          }}
          autofocus
        />
      </Field>
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
      <div class="modal-actions">
        <button
          class="btn btn-primary btn-sm"
          disabled={saving() || problem() != null}
          onClick={() => void save()}
        >
          {saving() ? "Assigning…" : "Assign"}
        </button>
        <button class="btn btn-ghost btn-sm" onClick={() => props.onCancel()}>
          Cancel
        </button>
      </div>
    </div>
  );
}

/**
 * App preflight (git-bug 90644c6) — the credential health line, one level
 * deeper: not "does the credential resolve" but "is the app it belongs to
 * *allowed* to do what Scarab will ask of it".
 *
 * It sits here, next to the credential line, because both failures it catches
 * are otherwise **invisible**. An App subscribed to no events still delivers
 * `installation`/`installation_repositories`, so the connection registers
 * itself, its repositories can be enabled, and nothing ever builds. An App without
 * `statuses:write` runs pipelines happily and silently never posts a check. In
 * both cases the operator's only signal today is an absence.
 *
 * One live forge round-trip per GitHub connection, fired on render: the whole
 * point is that a misconfigured App shows as unhealthy *without* being asked, so
 * a button behind which the truth hides would defeat it. The row renders
 * immediately and this line fills in.
 *
 * Three states, never two — `unknown` (an adapter that cannot introspect, a
 * credential that does not resolve, a forge that errored) must not render as
 * "you are fine". When we could not look, the requirement list is shown instead,
 * so the answer is still actionable.
 */
export function AppPreflight(props: { conn: Connection }) {
  const [report] = createResource(() => props.conn.id, connectionPreflight);
  const required = () => report()?.missing.filter((g) => g.severity === "required") ?? [];
  const recommended = () => report()?.missing.filter((g) => g.severity === "recommended") ?? [];

  return (
    <div class="conn-preflight">
      <Show when={report.loading}>
        <span class="subtle">
          <small>checking the app's permissions and event subscription…</small>
        </span>
      </Show>
      <Show when={report.error}>
        <span class="subtle">
          <small>could not check the app's configuration.</small>
        </span>
      </Show>
      <Show when={report()}>
        {(r) => (
          <>
            <Show when={r().status === "ok"}>
              <span class="conn-ok">
                <Icon icon="shield-check" size={12} /> app configuration matches what Scarab needs
              </span>
            </Show>
            <Show when={r().status === "unknown"}>
              <span class="subtle">
                <Icon icon="circle-dot" size={12} /> app configuration not checked —{" "}
                {r().unavailable_reason ?? "the forge could not be asked"}
              </span>
            </Show>
            <Show when={r().status === "degraded"}>
              <span class="conn-bad">
                <Icon icon="alert-triangle" size={12} />{" "}
                {required().length > 0
                  ? `app is missing ${required().length} setting${required().length === 1 ? "" : "s"} Scarab needs`
                  : "app configuration is incomplete"}
              </span>
            </Show>
            {/* The gap list is the actionable half: what to change, and what is
                silently broken until it is. Recommended gaps are listed on a
                degraded connection too — an operator fixing one setting on
                GitHub should fix them all in the same visit — but never on
                their own, or the line cries wolf. */}
            <Show when={r().status === "degraded"}>
              <ul class="preflight-gaps">
                <For each={[...required(), ...recommended()]}>
                  {(gap) => <PreflightGap gap={gap} />}
                </For>
              </ul>
            </Show>
            {/* Nothing could be checked: say what Scarab needs, so the answer is
                still worth having. */}
            <Show when={r().status === "unknown" && r().required.length > 0}>
              <ul class="preflight-gaps">
                <For each={r().required.filter((g) => g.severity === "required")}>
                  {(gap) => <PreflightGap gap={gap} muted />}
                </For>
              </ul>
            </Show>
          </>
        )}
      </Show>
    </div>
  );
}

/** One capability: what to turn on, and what stays broken until it is. */
function PreflightGap(props: { gap: CapabilityRequirement; muted?: boolean }) {
  const label = () =>
    props.gap.kind === "event"
      ? `${props.gap.name} event`
      : `${props.gap.name}: ${props.gap.level ?? "read"}`;
  return (
    <li classList={{ "preflight-gap": true, "preflight-gap-muted": props.muted }}>
      <span classList={{ "preflight-sev": true, req: props.gap.severity === "required" }}>
        {props.gap.severity}
      </span>
      <code class="mono">{label()}</code>
      <span class="subtle">{props.gap.why}</span>
    </li>
  );
}
