// Forge connections — the Settings section that answers "is my
// forge still wired up, and what does it cover?"
//
// A connection is the thing that makes a repo a Project: `ForgeConnection`
// resolves a forge coordinate to a governed Project, and a Project *is* that
// binding. So this list is the whole footprint of Scarab's reach.
//
// The surface is **asymmetric by forge**, and that asymmetry is the design:
//
// - **GitHub is observe-only.** Installing the App *is* registration — there is
//   no API for Scarab to install it, uninstall it, or change which repos it
//   covers. Pretending otherwise would mean a create form that cannot work. What
//   the UI can honestly offer is a deep link to the place that does own it, and a
//   **re-sync** for the one failure mode Scarab can fix by itself: a dropped
//   `installation_repositories` delivery leaving a covered repo with no Project.
// - **Forgejo is fully in-product.** It emits no installation event, so an admin
//   adds the connection here (base URL + token) — the only route that existed
//   before was a hand-written database row.
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
import { useNavigate } from "@solidjs/router";
import {
  assignConnectionOrg,
  availableRepos,
  bindRepo,
  connectionPreflight,
  createConnection,
  deleteConnection,
  listConnections,
  registerRepoWebhook,
  resyncConnection,
  unbindRepo,
  type CapabilityRequirement,
  type Connection,
} from "../api/client";
import {
  orgNameProblem,
  reposForOrg,
  resyncNote,
  soleOwner,
  unassignedConnections,
} from "../connections";
import { relTime } from "../fmt";
import { me, refetchMe } from "../session";
import { FIRST_ORG_SECTION, orgHref } from "../settings-nav";
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
                  installation webhook registers the connection and its repositories. For Forgejo
                  or Codeberg, add the connection below.
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
                <span class="mono">{props.org}</span>.
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
                organization its own connection (for GitHub, install the App on that account; for
                Forgejo, add one on its Connections page) and re-sync it there: the repositories
                move to it with their projects. Then remove this one.
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
 * Connect a forge: a new org's first connection, for a global admin. An org
 * comes into being with its first connection, so this cannot live in any
 * org's own settings. On success it lands on the new org's Connections page.
 */
export function ConnectForge() {
  const nav = useNavigate();
  return (
    <div class="panel">
      <div class="panel-h"><span>New organization</span></div>
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
 * read-only; without it, the caller names a new org. With `onCancel` it is a
 * dialog: the same sections in a Modal, the actions in its footer.
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
          purpose="Connect a Forgejo or Codeberg instance. Its repositories become projects once you add them."
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
  const [adding, setAdding] = createSignal(false);
  const [removing, setRemoving] = createSignal(false);
  const [assigning, setAssigning] = createSignal(false);

  // Which connections accept in-product repo management. GitHub does not: its
  // repo selection lives on GitHub (the App's coverage decides it), so an add or
  // remove here would be undone by the next `installation_repositories`
  // delivery. Config-owned connections are read-only wherever they come from.
  // An unassigned connection serves no org, so there is no owner to bind
  // repositories for: it offers assign and remove only.
  const manual = () => c().kind !== "github" && !c().managed_by_config && c().org != null;

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

  // Register (or re-register) one repo's forge-side hook. The retry for the one
  // onboarding step that depends on the forge being reachable right now.
  const registerWebhook = async (owner: string, name: string) => {
    setBusy(true);
    setNote(null);
    setError(null);
    try {
      await registerRepoWebhook(c().id, owner, name);
      setNote(`Webhook registered for ${owner}/${name}.`);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not register the webhook.");
    } finally {
      setBusy(false);
    }
  };

  // Unbinding removes the Project, so it takes that repo's environments, secrets
  // and access grants with it. The confirm says so; a refusal shows in it.
  const [removingRepo, setRemovingRepo] = createSignal<
    { org: string; project: string; owner: string; name: string } | null
  >(null);
  const removeRepo = async (owner: string, name: string) => {
    setBusy(true);
    setNote(null);
    setError(null);
    try {
      await unbindRepo(c().id, owner, name);
      props.onChanged();
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
      await deleteConnection(c().id, c().projects.length > 0);
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
            title="Ask the forge which repositories this connection covers and bind any that are missing"
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

      <div class="conn-projects">
        <Show
          when={c().projects.length > 0}
          fallback={<span class="subtle">No repositories bound yet.</span>}
        >
          <For each={c().projects}>
            {(p) => (
              <span class="conn-proj-row">
                <a class="conn-proj mono" href={`/${p.org}/${p.project}`}>
                  {p.org}/{p.project}
                </a>
                {/* Manual repo management only where it is real. GitHub's repo
                    selection lives on GitHub — the App's coverage decides it, and
                    an unbind here would just be undone by the next
                    `installation_repositories` delivery. */}
                <Show when={manual()}>
                  <button
                    class="btn btn-ghost btn-sm"
                    disabled={busy()}
                    title="Register (or re-register) this repository's webhook on the forge"
                    onClick={() => registerWebhook(p.owner, p.name)}
                  >
                    <Icon icon="rotate-cw" size={12} /> Webhook
                  </button>
                  <button
                    class="btn btn-danger btn-sm"
                    disabled={busy()}
                    title="Remove this repository's project from Scarab"
                    onClick={() => setRemovingRepo({ org: p.org, project: p.project, owner: p.owner, name: p.name })}
                  >
                    Remove
                  </button>
                </Show>
              </span>
            )}
          </For>
        </Show>
      </div>

      <Show when={manual()}>
        <div class="secrets-actions">
          <button class="btn btn-ghost btn-sm" disabled={busy()} onClick={() => setAdding(true)}>
            <Icon icon="plus" size={13} /> Add repository
          </button>
        </div>
      </Show>

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
      <Show when={removingRepo()}>
        {(r) => (
          <ConfirmDestructive
            title={`Remove ${r().org}/${r().project} from Scarab?`}
            purpose="Its project — including environments, secrets and access grants — is removed with it."
            name={`${r().org}/${r().project}`}
            confirmLabel="Remove repository"
            onConfirm={() => removeRepo(r().owner, r().name)}
            onClose={() => setRemovingRepo(null)}
          >
            <p>The repository itself and its webhook are untouched.</p>
          </ConfirmDestructive>
        )}
      </Show>

      <Show when={note()}>
        <p class="subtle"><small>{note()}</small></p>
      </Show>
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>

      <Show when={adding()}>
        <AddRepoDialog
          conn={c()}
          onClose={() => setAdding(false)}
          onAdded={(msg) => {
            setNote(msg);
            props.onChanged();
          }}
        />
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
  const projects = () => props.conn.projects;
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
        <Show when={projects().length > 0} fallback="It serves no projects.">
          This also removes {projects().length}{" "}
          {projects().length === 1 ? "project" : "projects"} (
          <span class="mono">{projects().map((p) => `${p.org}/${p.project}`).join(", ")}</span>)
          with their environments, secrets and access grants.
        </Show>
      </p>
    </ConfirmDestructive>
  );
}

/**
 * Assign an unassigned connection to an org, for a global admin. The org is
 * the account that owns the repositories bound through it, so the field
 * starts with that owner when there is one. The server refuses an org that
 * does not own every bound repository, or one served by another forge, and
 * says why; the reason is shown as it comes.
 */
function AssignConnection(props: {
  conn: Connection;
  onAssigned: (org: string) => void;
  onCancel: () => void;
}) {
  const [org, setOrg] = createSignal(soleOwner(props.conn.projects) ?? "");
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
 * itself, the projects list fills in, and nothing ever builds. An App without
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

/**
 * Bring repos on a connection under governance. Binding **creates the Project**
 * (there is no `projects` table — a Project is the binding), which is
 * why this dialog is the Forgejo onboarding flow and not a convenience.
 *
 * The pick-list comes from the forge, so nobody types `owner/name`. When the
 * adapter cannot enumerate, the field is offered instead — "I cannot look" and
 * "your token reaches nothing" must not render the same.
 *
 * It offers only repositories owned by the connection's org: the server
 * refuses any other owner (400), and when it does, its message is shown as is.
 */
function AddRepoDialog(props: {
  conn: Connection;
  onClose: () => void;
  onAdded: (note: string) => void;
}) {
  const [avail] = createResource(() => props.conn.id, availableRepos);
  const org = () => props.conn.org ?? "";
  const pick = () => reposForOrg(avail() ?? [], props.conn.org);
  const [manualRepo, setManualRepo] = createSignal("");
  const [busy, setBusy] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);

  const add = async (owner: string, name: string) => {
    setBusy(`${owner}/${name}`);
    setError(null);
    try {
      const r = await bindRepo(props.conn.id, { owner, name });
      props.onAdded(
        r.webhook_registered
          ? `Added ${r.org}/${r.project} and registered its webhook.`
          : `Added ${r.org}/${r.project}, but the webhook could not be registered: ${r.webhook_error ?? "unknown reason"}. Pushes will not start runs until it is.`,
      );
      props.onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not add the repository.");
    } finally {
      setBusy(null);
    }
  };

  const addTyped = () => {
    const [owner, name] = manualRepo().trim().split("/");
    if (!owner || !name) {
      setError("Enter the repository as owner/name.");
      return;
    }
    void add(owner, name);
  };

  // The forge cannot list its repositories, so the dialog asks for one by name
  // and its footer carries Add.
  const typing = () => !avail.loading && avail() == null;

  return (
    <Modal
      eyebrow={`${org()} · ${props.conn.id}`}
      title="Add repository"
      purpose="Adding a repository creates its project and registers a webhook, so pushes start runs."
      busy={busy() != null}
      onClose={props.onClose}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose}>
            Cancel
          </button>
          <Show when={typing()}>
            <button class="btn btn-primary" type="button" disabled={busy() != null} onClick={addTyped}>
              <Icon icon="plus" size={14} /> Add
            </button>
          </Show>
        </>
      }
    >
      <Show when={avail.error}>
        <p class="error">Could not ask the forge which repositories it can see.</p>
      </Show>
      <Show when={!avail.loading} fallback={<p class="empty">asking the forge…</p>}>
        {/* `null` = this adapter cannot enumerate. Fall back to typing it. */}
        <Show
          when={avail() != null}
          fallback={
            <Field
              label="Repository"
              required
              help={
                <>
                  This forge cannot list its repositories, so name one directly. It must be owned
                  by <span class="mono">{org()}</span>.
                </>
              }
            >
              <Input mono autofocus placeholder={`${org()}/name`} value={manualRepo()} onValue={setManualRepo} />
            </Field>
          }
        >
          <Show
            when={pick().offered.length > 0}
            fallback={
              <p class="empty">
                <Show
                  when={pick().hidden > 0}
                  fallback="This connection's token does not reach any repository. Check its scopes on the forge."
                >
                  This connection's token reaches no repository owned by{" "}
                  <span class="mono">{org()}</span>.
                </Show>
              </p>
            }
          >
            <ul class="secret-list">
              <For each={pick().offered}>
                {(r) => (
                  <li class="secret-row">
                    <Icon icon="git-branch" size={14} />
                    <code class="mono">
                      {r.owner}/{r.name}
                    </code>
                    <Show
                      when={!r.bound}
                      fallback={<span class="subtle"><small>already added</small></span>}
                    >
                      <button
                        class="btn btn-primary btn-sm"
                        disabled={busy() != null}
                        onClick={() => add(r.owner, r.name)}
                      >
                        {busy() === `${r.owner}/${r.name}` ? "Adding…" : "Add"}
                      </button>
                    </Show>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </Show>
      </Show>
      <Show when={pick().hidden > 0}>
        <p class="f-help">
          {pick().hidden} {pick().hidden === 1 ? "repository" : "repositories"} the token
          reaches {pick().hidden === 1 ? "is" : "are"} owned by other accounts and not
          listed. This connection binds only repositories owned by{" "}
          <span class="mono">{org()}</span>.
        </p>
      </Show>
    </Modal>
  );
}
