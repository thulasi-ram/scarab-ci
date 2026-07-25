// Forge connections (ADR-0060 part C) — the Settings section that answers "is my
// forge still wired up, and what does it cover?"
//
// A connection is the thing that makes a repo a Project: `ForgeConnection`
// resolves a forge coordinate to a governed Project, and a Project *is* that
// binding (ADR-0046). So this list is the whole footprint of Scarab's reach.
//
// The surface is **asymmetric by forge**, and that asymmetry is the design
// (ADR-0060 part C):
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
import { createResource, createSignal, For, Show } from "solid-js";
import {
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
import { relTime } from "../fmt";
import Icon from "./Icon";

/** Where a connection's own management lives, when the forge has such a page. */
function manageUrl(c: Connection): string | null {
  // GitHub App installations are managed per-account on github.com; the exact
  // installation id isn't in our model, so link to the account's app settings
  // where the install is listed.
  return c.kind === "github" ? `${c.web_url}/settings/installations` : null;
}

export default function Connections() {
  const [epoch, setEpoch] = createSignal(0);
  const [conns] = createResource(epoch, listConnections);
  const [adding, setAdding] = createSignal(false);
  const reload = () => setEpoch((e) => e + 1);

  return (
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
                No forge connected yet. For GitHub, install the Scarab App on your account or
                organization — the installation webhook registers the connection and its
                repositories automatically. For Forgejo or Codeberg, add the connection below.
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
              GitHub connections register themselves when the App is installed — there is
              nothing to add here for them.
            </small>
          </p>
        </Show>
      </div>
      <Show when={adding()}>
        <NewForgejoConnectionDialog onClose={() => setAdding(false)} onSaved={reload} />
      </Show>
    </div>
  );
}

/**
 * Add a Forgejo/Codeberg connection: an instance URL and an access token.
 *
 * Forgejo-only by construction — a GitHub form would be a lie (the server
 * refuses it, because installing the App is the registration). The token field is
 * `type="password"` and write-only end to end: it is posted once and the server
 * stores it under a handle it generates itself, so there is no code path, here or
 * on the server, that can render it back.
 */
function NewForgejoConnectionDialog(props: { onClose: () => void; onSaved: () => void }) {
  const [baseUrl, setBaseUrl] = createSignal("");
  const [token, setToken] = createSignal("");
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const ready = () => baseUrl().trim().length > 0 && token().trim().length > 0;

  const save = async () => {
    if (!ready()) return;
    setSaving(true);
    setError(null);
    try {
      await createConnection({
        kind: "forgejo",
        base_url: baseUrl().trim(),
        credential: token().trim(),
      });
      props.onSaved();
      props.onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not create the connection.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h"><span>Add Forgejo connection</span></div>
        <div class="modal-body">
          <div class="form-r">
            <label>instance URL</label>
            <input
              class="input"
              placeholder="https://codeberg.org"
              value={baseUrl()}
              onInput={(e) => setBaseUrl(e.currentTarget.value)}
            />
          </div>
          <div class="form-r">
            <label>access token</label>
            <input
              class="input"
              type="password"
              placeholder="token (write-only)"
              value={token()}
              onInput={(e) => setToken(e.currentTarget.value)}
            />
          </div>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={saving() || !ready()} onClick={save}>
              <Icon icon="plus" size={14} /> {saving() ? "Connecting…" : "Add connection"}
            </button>
            <button class="btn btn-ghost" onClick={props.onClose}>Cancel</button>
          </div>
          <p class="subtle modal-note">
            <small>
              The token needs repository read plus webhook administration on the repos you
              intend to build. It is stored encrypted and never displayed again.
            </small>
          </p>
        </div>
      </div>
    </div>
  );
}

function ConnectionRow(props: { conn: Connection; onChanged: () => void }) {
  const c = () => props.conn;
  const [busy, setBusy] = createSignal(false);
  const [note, setNote] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [adding, setAdding] = createSignal(false);

  // Which connections accept in-product repo management. GitHub does not: its
  // repo selection lives on GitHub (the App's coverage decides it), so an add or
  // remove here would be undone by the next `installation_repositories`
  // delivery. Config-owned connections are read-only wherever they come from.
  const manual = () => c().kind !== "github" && !c().managed_by_config;

  const resync = async () => {
    setBusy(true);
    setNote(null);
    setError(null);
    try {
      const r = await resyncConnection(c().id);
      setNote(
        r.bound.length > 0
          ? `Bound ${r.bound.length} new ${r.bound.length === 1 ? "repository" : "repositories"}: ${r.bound.join(", ")}`
          : `Already in sync — ${r.confirmed} ${r.confirmed === 1 ? "repository" : "repositories"} confirmed.`,
      );
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
  // and access grants with it. Say so before doing it.
  const removeRepo = async (org: string, project: string, owner: string, name: string) => {
    if (
      !window.confirm(
        `Remove ${org}/${project} from Scarab?\n\nIts project — including environments, secrets and access grants — is removed with it. The repository itself and its webhook are untouched.`,
      )
    )
      return;
    setBusy(true);
    setNote(null);
    setError(null);
    try {
      await unbindRepo(c().id, owner, name);
      props.onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove the repository.");
    } finally {
      setBusy(false);
    }
  };

  // Removing a connection removes the Projects it serves — a Project *is* the
  // binding — so the server refuses unless we acknowledge that explicitly. Name
  // what goes in the confirm rather than sending the acknowledgement blindly.
  const remove = async () => {
    const projects = c().projects;
    const warning =
      projects.length > 0
        ? `Remove the ${c().kind} connection to ${c().base_url}?\n\nThis also removes ${projects.length} project(s) — ${projects
            .map((p) => `${p.org}/${p.project}`)
            .join(", ")} — along with their environments, secrets and access grants.`
        : `Remove the ${c().kind} connection to ${c().base_url}?`;
    if (!window.confirm(warning)) return;
    setBusy(true);
    setNote(null);
    setError(null);
    try {
      await deleteConnection(c().id, projects.length > 0);
      props.onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove the connection.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <li class="conn">
      <div class="conn-head">
        <span class="conn-kind mono">{c().kind}</span>
        <code class="mono conn-base">{c().base_url}</code>
        {/* Config-owned connections are read-only here on purpose (ADR-0060 part
            D): the `connections:` block is authoritative, so an edit made here
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
        <Show when={c().supports_resync && !c().managed_by_config}>
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
            disabled={busy()}
            title="Remove this connection from Scarab"
            onClick={remove}
          >
            Remove
          </button>
        </Show>
      </div>

      <div class="conn-facts">
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
                    onClick={() => removeRepo(p.org, p.project, p.owner, p.name)}
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
 * (there is no `projects` table — a Project is the binding, ADR-0046), which is
 * why this dialog is the Forgejo onboarding flow and not a convenience.
 *
 * The pick-list comes from the forge, so nobody types `owner/name`. When the
 * adapter cannot enumerate, the field is offered instead — "I cannot look" and
 * "your token reaches nothing" must not render the same.
 */
function AddRepoDialog(props: {
  conn: Connection;
  onClose: () => void;
  onAdded: (note: string) => void;
}) {
  const [avail] = createResource(() => props.conn.id, availableRepos);
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

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h"><span>Add repository</span></div>
        <div class="modal-body">
          <Show when={avail.error}>
            <p class="error">Could not ask the forge which repositories it can see.</p>
          </Show>
          <Show when={!avail.loading} fallback={<p class="empty">asking the forge…</p>}>
            {/* `null` = this adapter cannot enumerate. Fall back to typing it. */}
            <Show
              when={avail() != null}
              fallback={
                <>
                  <p class="subtle">
                    <small>
                      This forge cannot list its repositories, so name one directly.
                    </small>
                  </p>
                  <div class="form-r">
                    <label>repository</label>
                    <input
                      class="input"
                      placeholder="owner/name"
                      value={manualRepo()}
                      onInput={(e) => setManualRepo(e.currentTarget.value)}
                    />
                  </div>
                  <div class="modal-actions">
                    <button class="btn btn-primary" disabled={busy() != null} onClick={addTyped}>
                      <Icon icon="plus" size={14} /> Add
                    </button>
                    <button class="btn btn-ghost" onClick={props.onClose}>Cancel</button>
                  </div>
                </>
              }
            >
              <Show
                when={(avail()?.length ?? 0) > 0}
                fallback={
                  <p class="empty">
                    This connection's token does not reach any repository. Check its scopes on
                    the forge.
                  </p>
                }
              >
                <ul class="secret-list">
                  <For each={avail()!}>
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
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <p class="subtle modal-note">
            <small>
              Adding a repository creates its project and registers a webhook, so pushes
              start runs.
            </small>
          </p>
        </div>
      </div>
    </div>
  );
}
