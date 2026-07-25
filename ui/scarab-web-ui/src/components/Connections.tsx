// Forge connections (ADR-0060 part C) — the Settings section that answers "is my
// forge still wired up, and what does it cover?"
//
// A connection is the thing that makes a repo a Project: `ForgeConnection`
// resolves a forge coordinate to a governed Project, and a Project *is* that
// binding (ADR-0046). So this list is the whole footprint of Scarab's reach.
//
// GitHub is **observe-only** on purpose. Installing the App *is* registration —
// there is no API for Scarab to install it, uninstall it, or change which repos
// it covers. Pretending otherwise would mean a create form that cannot work. What
// the UI can honestly offer is a deep link to the place that does own it, and a
// **re-sync** for the one failure mode Scarab can fix by itself: a dropped
// `installation_repositories` delivery leaving a covered repo with no Project.
import { createResource, createSignal, For, Show } from "solid-js";
import { listConnections, resyncConnection, type Connection } from "../api/client";
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
                No forge connected yet. Install the Scarab GitHub App on your account or
                organization — the installation webhook registers the connection and its
                repositories automatically.
              </p>
            }
          >
            <ul class="conn-list">
              <For each={conns()}>
                {(c) => <ConnectionRow conn={c} onChanged={() => setEpoch((e) => e + 1)} />}
              </For>
            </ul>
          </Show>
        </Show>
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
