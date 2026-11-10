// Forge connections — the Settings section that answers "is my
// forge still wired up, and what does it cover?"
//
// A connection is the thing that makes a repo a Project: `ForgeConnection`
// resolves a forge coordinate to a governed Project, and a Project *is* that
// binding. So this list is the whole footprint of Scarab's reach.
//
// The connection itself differs by forge:
//
// - **GitHub** connections register themselves: installing the App creates
//   the connection. Scarab cannot install, uninstall or change which repos it
//   covers, so the row links to GitHub for that.
// - **Forgejo** emits no installation event, so an admin adds the connection
//   here (base URL + token). This is the one place a Forgejo connection is
//   added: Add Forgejo connection on an org's Connections, or, for an org that
//   does not exist yet, the New organization page (`NewOrgConnection`), which
//   shows the same form with a field for the org's name.
//
// Repositories are opt-in for both, and they are enabled and disabled in one
// place: the org page (`/orgs/<org>`), which pages and filters them. A
// connection here shows how many repositories it has enabled and links to the
// org page narrowed to it (Manage repositories); it lists none of them, since
// a connection can reach thousands. Re-sync asks the forge what it reaches and
// lists what is not enabled.
//
// The token is write-only: it is typed once, posted once, and never rendered.
// The row reports whether the *handle* resolves, which is the question an
// operator actually has ("is my forge still wired up?").
//
// Each connection serves one org (git-bug 19b0a45): the forge account that owns
// the repositories it binds. This section lists the connections of the org in
// the URL. A connection that serves no org yet ("unassigned") is listed to a
// global admin in a panel of its own, on every org's page, because no org's
// list includes it.
import { createResource, createSignal, For, Show, type JSX } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import {
  assignConnectionOrg,
  connectionPreflight,
  createConnection,
  deleteConnection,
  listConnections,
  resyncConnection,
  type CapabilityRequirement,
  type Connection,
} from "../api/client";
import { orgNameProblem, resyncNote, unassignedConnections } from "../connections";
import { relTime } from "../fmt";
import { me, refetchMe } from "../session";
import { FIRST_ORG_SECTION, orgHref } from "../settings-nav";
import { orgPageHref } from "../org-page";
import FormSection from "./FormSection";
import Modal, { ModalMessage, ModalSection } from "./Modal";
import ConfirmDestructive from "./ConfirmDestructive";
import { Field, Input } from "./Field";
import Icon from "./Icon";

/** Where a connection's own management lives, when the forge has such a page. */
function manageUrl(c: Connection): string | null {
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

  return (
    <>
      <div class="panel">
        <div class="panel-h"><span>Forge connections</span></div>
        <div class="secrets-body">
          <Show when={conns.error}>
            <p class="error">Could not load connections.</p>
          </Show>
          <Show when={!conns.loading} fallback={<p class="empty">loading…</p>}>
            <Show
              when={(conns()?.length ?? 0) > 0}
              fallback={
                <p class="empty">
                  No forge connected to <b class="mono">{props.org}</b> yet. For GitHub, install
                  the Scarab App on the <span class="mono">{props.org}</span> account. The
                  installation webhook registers the connection; you then enable its repositories
                  on the <A href={orgPageHref(props.org)}>organization page</A>. For Forgejo or
                  Codeberg, add the connection below.
                </p>
              }
            >
              <ul class="conn-list">
                <For each={conns()}>
                  {(c) => <ConnectionRow conn={c} onChanged={reload} />}
                </For>
              </ul>
            </Show>
            <div class="secrets-actions">
              <button class="btn btn-primary btn-sm" onClick={() => setAdding(true)}>
                <Icon icon="plus" size={14} /> Add Forgejo connection
              </button>
            </div>
            <p class="subtle">
              <small>
                GitHub connections register themselves when the App is installed, so there is
                nothing to add here for them. A connection binds only repositories owned by{" "}
                <span class="mono">{props.org}</span>, and only the ones you enable: see the{" "}
                <A href={orgPageHref(props.org)}>organization page</A>.
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
                with their projects. Then remove this one. For GitHub, install the App on that
                account. For Forgejo, use Add Forgejo connection on that organization's
                Connections, or New organization… in the organization menu above if Scarab does
                not have it yet.
              </small>
            </p>
            <ul class="conn-list">
              <For each={unassigned()}>
                {(c) => <ConnectionRow conn={c} onChanged={reload} />}
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

function ConnectionRow(props: { conn: Connection; onChanged: () => void }) {
  const c = () => props.conn;
  const [busy, setBusy] = createSignal(false);
  const [note, setNote] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [removing, setRemoving] = createSignal(false);
  const [assigning, setAssigning] = createSignal(false);

  const resync = async () => {
    setBusy(true);
    setNote(null);
    setError(null);
    try {
      const r = await resyncConnection(c().id);
      setNote(resyncNote(r));
      props.onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Re-sync failed.");
    } finally {
      setBusy(false);
    }
  };

  // Removing a connection removes the Projects it serves — a Project *is* the
  // binding — so the server refuses unless we acknowledge that explicitly. The
  // confirm names what goes and asks for the connection's id to be typed; a
  // refusal shows in it.
  const remove = async () => {
    setBusy(true);
    setNote(null);
    setError(null);
    try {
      await deleteConnection(c().id, c().enabled_repos > 0);
      props.onChanged();
    } finally {
      setBusy(false);
    }
  };

  return (
    <li class="conn">
      <div class="conn-head">
        <span class="conn-kind mono">{c().kind}</span>
        <code class="mono conn-base">{c().base_url}</code>
        {/* Config-owned connections are read-only here on purpose: the
            `connections:` block is authoritative, so an edit made here
            would be silently reverted on the next boot. The tag names the source
            so the change has an obvious home. */}
        <Show when={c().managed_by_config}>
          <span
            class="conn-tag"
            title="Provisioned from the server's connections: configuration (Helm scarab.connections / SCARAB_CONNECTIONS) and authoritative there — this connection cannot be edited or removed here."
          >
            managed by configuration
          </span>
        </Show>
        <span class="topbar-spacer" />
        <Show when={manageUrl(c())}>
          {(href) => (
            <a class="btn btn-ghost btn-sm" href={href()} target="_blank" rel="noreferrer">
              Manage on {c().kind === "github" ? "GitHub" : "the forge"} →
            </a>
          )}
        </Show>
        <Show when={c().org == null && !c().managed_by_config}>
          <button
            class="btn btn-ghost btn-sm"
            disabled={busy() || assigning()}
            title="Choose the organization this connection serves"
            onClick={() => setAssigning(true)}
          >
            Assign to org
          </button>
        </Show>
        {/* An unassigned connection binds nothing on a re-sync, so it offers none. */}
        <Show when={c().supports_resync && !c().managed_by_config && c().org != null}>
          <button
            class="btn btn-ghost btn-sm"
            disabled={busy()}
            title="Ask the forge which repositories this connection reaches and list the ones not enabled"
            onClick={resync}
          >
            <Icon icon="rotate-cw" size={13} /> {busy() ? "Re-syncing…" : "Re-sync"}
          </button>
        </Show>
        <Show when={!c().managed_by_config}>
          <button
            class="btn btn-danger btn-sm"
            disabled={busy() || removing()}
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
        {/* Credential health. The most common silent breakage is a database
            restored without its secrets — invisible until a run fails. */}
        <span classList={{ "conn-ok": c().credential_present, "conn-bad": !c().credential_present }}>
          <Icon icon={c().credential_present ? "shield-check" : "alert-triangle"} size={12} />{" "}
          credential <code class="mono">{c().credential_ref}</code>{" "}
          {c().credential_present ? "resolves" : "MISSING — the forge cannot be reached"}
        </span>
        {/* `null` means no delivery has been recorded, which is unknown rather
            than broken — a brand-new connection has simply not been used yet. */}
        <span class="subtle">
          last delivery{" "}
          {c().last_delivery_at != null ? relTime(c().last_delivery_at!) : "— none recorded yet"}
        </span>
      </div>

      <Show when={c().supports_preflight}>
        <AppPreflight conn={c()} />
      </Show>

      {/* How many, not which: a connection can hold thousands. The org page
          lists them, pages and filters them, and enables and disables them. */}
      <div class="conn-projects">
        <span class="subtle">
          {c().enabled_repos} enabled {c().enabled_repos === 1 ? "repository" : "repositories"}
        </span>
        <Show when={c().org}>
          {(o) => (
            <A class="btn btn-ghost btn-sm" href={orgPageHref(o(), c().id)}>
              Manage repositories
            </A>
          )}
        </Show>
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
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
    </li>
  );
}

/**
 * The confirm for removing a connection: what goes with it, and its id typed
 * exactly. Removing it removes every Project it serves.
 */
function RemoveConnection(props: {
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
function AppPreflight(props: { conn: Connection }) {
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
