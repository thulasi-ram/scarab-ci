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
  createConnection,
  deleteConnection,
  listConnections,
  resyncConnection,
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
        <Show when={c().managed_by_config}>
          <span class="conn-tag" title="Provisioned from server configuration — edit it there">
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

      <div class="conn-projects">
        <Show
          when={c().projects.length > 0}
          fallback={<span class="subtle">No repositories bound yet.</span>}
        >
          <For each={c().projects}>
            {(p) => (
              <a class="conn-proj mono" href={`/${p.org}/${p.project}`}>
                {p.org}/{p.project}
              </a>
            )}
          </For>
        </Show>
      </div>

      <Show when={note()}>
        <p class="subtle"><small>{note()}</small></p>
      </Show>
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
    </li>
  );
}
