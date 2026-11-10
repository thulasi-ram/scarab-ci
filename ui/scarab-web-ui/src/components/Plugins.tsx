// Plugins — the Settings section where an org admin installs a plugin, sees
// whether it is running, kills it, and binds its connections.
//
// A PluginInstallation is the approval of exactly (image digest, manifest).
// Every plugin installs the same way: the admin names the manifest's git
// source, the server reads it (and stores nothing), the dialog shows
// everything it asks for, and Install sends the reviewed manifest's hash with
// an image pinned by digest. Who publishes a plugin changes nothing: it is
// untrusted code either way. A refusal from the server is shown verbatim. Kill revokes at the token-to-installation mapping, so it
// bites on the next request rather than when a token expires. Uninstall
// retires the name forever.
//
// "Connections" bind each manifest connection slot to the
// ONE destination the CredentialBroker sends the plugin's reads to, and the
// credential it adds. The credential is write-only — the panel only ever says
// "set" / "not set" — and is typed again on every save, so a destination
// never inherits a credential entered for another. Plain http is allowed but
// always flagged.
//
// "Broker journal" lists the requests the plugin sent to the broker in the
// last 14 days, newest first: what was forwarded, what was refused and why.
// Query values are never shown, only the parameter names.
//
// "Storage" names one PersistentVolumeClaim an operator created
// in the plugin namespace; the runtime mounts it read-write into the plugin's
// Pod. Scarab does not create, back up or read it. Moving it to another claim
// or path asks for a step-up; adding or removing it does not.
import { createEffect, createResource, createSignal, For, on, Show } from "solid-js";
import {
  deletePluginConnection,
  deletePluginStorage,
  enablePlugin,
  fetchPluginManifest,
  getPluginBrokerJournal,
  installPlugin,
  killPlugin,
  listConnections,
  listPluginConnections,
  listPlugins,
  putPluginConnection,
  putPluginStorage,
  uninstallPlugin,
  type BrokerJournalEntry,
  type FetchedPluginManifest,
  type PluginConnection,
  type PluginInstallation,
} from "../api/client";
import {
  appendJournalPage,
  connectionBaseUrlError,
  credentialBadge,
  journalEmptyText,
  journalEntryView,
  journalParams,
  type JournalFilter,
  type JournalPhaseFilter,
  imageError,
  imageRepository,
  INSECURE_HTTP_WARNING,
  isInsecureHttp,
  manifestReview,
  manifestSourceError,
  suggestedInstallName,
  toConnectionBody,
  toManifestFetchBody,
  type ManifestFetchForm,
  type ConnectionForm,
  installNameError,
  runtimeBadge,
  shortDigest,
  stateBadge,
  toInstallBody,
  ruleLabel,
  DEFAULT_STORAGE_MOUNT_PATH,
  STORAGE_NOTE,
  toStorageBody,
  type BadgeTone,
  type StorageForm,
} from "../plugins";
import { relTime, absTime } from "../fmt";
import Icon from "./Icon";
import Modal, { ModalMessage, ModalSection } from "./Modal";
import ConfirmDestructive, { ConfirmAction } from "./ConfirmDestructive";
import { Checkbox, Field, Input, Select, Textarea } from "./Field";

/** A badge tone in the StatusBadge palette's classes. */
const BADGE: Record<BadgeTone, string> = {
  ok: "badge badge-succeeded",
  motion: "badge badge-running",
  danger: "badge badge-failed",
  muted: "badge badge-skipped",
};

export default function Plugins(props: { org: string }) {
  const [epoch, setEpoch] = createSignal(0);
  const [plugins] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listPlugins(k.org),
  );
  const [installing, setInstalling] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const reload = () => setEpoch((e) => e + 1);

  const unavailable = () => !plugins.loading && !plugins.error && plugins() === null;
  const rows = () => plugins() ?? [];

  const act = async (what: () => Promise<unknown>, fallback: string, done?: string) => {
    setError(null);
    setNotice(null);
    try {
      await what();
      if (done) setNotice(done);
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : fallback);
    }
  };

  // The installation a confirm dialog is open for. A refusal shows in the
  // dialog.
  const [killing, setKilling] = createSignal<PluginInstallation | null>(null);
  const [uninstalling, setUninstalling] = createSignal<PluginInstallation | null>(null);
  const done = (msg: string) => {
    setError(null);
    setNotice(msg);
    reload();
  };

  const enable = (p: PluginInstallation) =>
    void act(() => enablePlugin(props.org, p.name), `Could not enable ${p.name}.`, `${p.name} is enabled again.`);


  return (
    <div class="panel">
      <div class="panel-h">
        <span>Plugins · {props.org}</span>
        <Show when={rows().length > 0}>
          <span class="tok-count">{rows().length}</span>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={plugins.error}>
          <p class="error">Could not load the org's plugins.</p>
        </Show>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
        <Show when={notice()}>
          <p class="notice-ok">{notice()}</p>
        </Show>

        <Show when={!plugins.loading} fallback={<p class="empty">loading…</p>}>
          <Show when={!unavailable()} fallback={<p class="empty">This server does not run plugins.</p>}>
            <Show
              when={rows().length > 0}
              fallback={
                <p class="empty">
                  No plugins installed. A plugin connects Scarab to a vendor tool, such as Argo CD.
                  To install one, enter the git source of its manifest and the image to run.
                </p>
              }
            >
              <ul class="tok-list">
                <For each={rows()}>
                  {(p) => (
                    <PluginRow
                      org={props.org}
                      plugin={p}
                      onKill={() => setKilling(p)}
                      onEnable={() => enable(p)}
                      onUninstall={() => setUninstalling(p)}
                      onStorageChanged={(msg) => {
                        setError(null);
                        setNotice(msg);
                        reload();
                      }}
                    />
                  )}
                </For>
              </ul>
            </Show>

            <div class="secrets-actions">
              <button class="btn btn-primary btn-sm" onClick={() => setInstalling(true)}>
                <Icon icon="plus" size={14} /> Install plugin
              </button>
            </div>
            <p class="subtle">
              <small>
                <Icon icon="shield-check" size={12} /> An installation approves one image digest
                and exactly what its manifest asks for — a new digest is a new install. A
                plugin acts as{" "}
                <span class="mono">plugin:&lt;name&gt;</span>, never as a person. Installing asks
                you to confirm with your passkey.
              </small>
            </p>
          </Show>
        </Show>
      </div>

      <Show when={installing()}>
        <InstallDialog
          org={props.org}
          taken={rows().map((p) => p.name)}
          onClose={() => setInstalling(false)}
          onInstalled={(p) => {
            setInstalling(false);
            setNotice(`${p.name} is installed.`);
            reload();
          }}
        />
      </Show>
      <Show when={killing()}>
        {(p) => (
          <ConfirmAction
            eyebrow={`${props.org} · plugins`}
            title={`Kill ${p().name}?`}
            purpose="Nothing is uninstalled — Enable brings it back."
            confirmLabel="Kill plugin"
            danger
            onConfirm={async () => {
              await killPlugin(props.org, p().name);
              done(`${p().name} is killed.`);
            }}
            onClose={() => setKilling(null)}
          >
            <p>
              This revokes it immediately: from the next request on, its broker calls are refused,
              even with a token that has not expired.
            </p>
          </ConfirmAction>
        )}
      </Show>
      <Show when={uninstalling()}>
        {(p) => (
          <ConfirmDestructive
            eyebrow={`${props.org} · plugins`}
            title={`Uninstall ${p().name}?`}
            purpose={`The name ${p().name} is retired forever — it can never be installed again.`}
            name={p().name}
            confirmLabel="Uninstall plugin"
            onConfirm={async () => {
              await uninstallPlugin(props.org, p().name);
              done(`${p().name} is uninstalled; its name is retired.`);
            }}
            onClose={() => setUninstalling(null)}
          />
        )}
      </Show>
    </div>
  );
}

/** One installation: identity, approval, runtime, and its connections. */
function PluginRow(props: {
  org: string;
  plugin: PluginInstallation;
  onKill: () => void;
  onEnable: () => void;
  onUninstall: () => void;
  onStorageChanged: (msg: string) => void;
}) {
  const p = () => props.plugin;
  const [storageOpen, setStorageOpen] = createSignal(false);
  const killed = () => p().state === "killed";
  const state = () => stateBadge(p().state);
  const runtime = () => runtimeBadge(p().runtime_status);
  const [connectionsOpen, setConnectionsOpen] = createSignal(false);
  const [journalOpen, setJournalOpen] = createSignal(false);

  return (
    <li class="tok" classList={{ "tok-dead": killed() }}>
      <div class="tok-head">
        <Icon icon="boxes" size={15} />
        <span class="tok-name mono">{p().name}</span>
        <span class="tok-scope mono" title="The plugin's name, from its manifest">
          {p().plugin}
        </span>
        <span class="tok-role" title="What it acts as in audit — not tied to any human">
          {p().principal}
        </span>
        <span class={`tok-state ${killed() ? "tok-revoked" : "plg-enabled"}`} title={state().title}>
          {state().label}
        </span>
        <span class="oenv-actions">
          <Show
            when={killed()}
            fallback={
              <button class="btn btn-danger btn-sm" onClick={props.onKill}>
                kill
              </button>
            }
          >
            <button class="btn btn-ghost btn-sm" onClick={props.onEnable}>
              enable
            </button>
          </Show>
          <button class="btn btn-ghost btn-sm" onClick={props.onUninstall}>
            uninstall
          </button>
        </span>
      </div>

      <div class="tok-facts">
        <span title={`${p().image}\nmanifest sha256:${p().manifest_sha256}`}>
          <Icon icon="container" size={12} />
          <span class="mono">
            {imageRepository(p().image)} · {shortDigest(p().image)}
          </span>
        </span>
        <span title={absTime(p().approved_at)}>
          <Icon icon="user" size={12} />
          approved by {p().approved_by} · {relTime(p().approved_at)}
        </span>
        <Show when={killed() && p().killed_at}>
          <span class="plg-killed" title={absTime(p().killed_at!)}>
            <Icon icon="alert-triangle" size={12} />
            killed by {p().killed_by ?? "?"} · {relTime(p().killed_at!)}
          </span>
        </Show>
      </div>

      <div class="tok-facts">
        <span title={runtime().title}>
          <span class={BADGE[runtime().tone]}>{runtime().label}</span>
          <Show when={p().runtime_message}>
            <span class="plg-msg">{p().runtime_message}</span>
          </Show>
        </span>
        <Show when={p().probe_passed_at}>
          {(at) => (
            <span title={`isolation probe passed ${absTime(at())}`}>
              <Icon icon="shield-check" size={12} />
              probe passed {relTime(at())}
            </span>
          )}
        </Show>
      </div>

      <div class="tok-facts">
        <span title={STORAGE_NOTE}>
          <Icon icon="database" size={12} />
          <Show when={p().storage} fallback={<span class="subtle">no storage</span>}>
            {(s) => (
              <span class="mono">
                {s().claim} → {s().mount_path}
              </span>
            )}
          </Show>
        </span>
        <button class="btn btn-ghost btn-sm plg-toggle" onClick={() => setStorageOpen((o) => !o)}>
          <Icon icon={storageOpen() ? "chevron-down" : "chevron-right"} size={12} /> storage
        </button>
      </div>

      <Show when={storageOpen()}>
        <StorageEditor
          org={props.org}
          plugin={p()}
          onChanged={(msg) => {
            setStorageOpen(false);
            props.onStorageChanged(msg);
          }}
        />
      </Show>

      <Show when={p().manifest.connections.length > 0}>
        <div class="tok-facts">
          <button class="btn btn-ghost btn-sm plg-toggle" onClick={() => setConnectionsOpen((o) => !o)}>
            <Icon icon={connectionsOpen() ? "chevron-down" : "chevron-right"} size={12} /> connections
          </button>
        </div>
      </Show>

      <Show when={connectionsOpen()}>
        <Connections org={props.org} installation={p().name} />
      </Show>

      <div class="tok-facts">
        <button class="btn btn-ghost btn-sm plg-toggle" onClick={() => setJournalOpen((o) => !o)}>
          <Icon icon={journalOpen() ? "chevron-down" : "chevron-right"} size={12} /> broker journal
        </button>
      </div>

      <Show when={journalOpen()}>
        <BrokerJournal org={props.org} installation={p().name} />
      </Show>
    </li>
  );
}

/** An installation's storage claim: what to tell the operator,
 *  and the form that sets, moves or removes it. */
function StorageEditor(props: {
  org: string;
  plugin: PluginInstallation;
  onChanged: (msg: string) => void;
}) {
  const current = () => props.plugin.storage ?? null;
  const [form, setForm] = createSignal<StorageForm>({
    claim: current()?.claim ?? "",
    mount_path: current()?.mount_path ?? "",
  });
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const built = () => toStorageBody(form());
  const set = (k: keyof StorageForm, v: string) => setForm((f) => ({ ...f, [k]: v }));
  const owner = () => `${props.plugin.org}/${props.plugin.name}`;

  const save = async () => {
    const body = built().body;
    if (!body || saving()) return;
    setSaving(true);
    setError(null);
    try {
      const updated = await putPluginStorage(props.org, props.plugin.name, body);
      const s = updated.storage;
      props.onChanged(
        s ? `${props.plugin.name} mounts ${s.claim} at ${s.mount_path}.` : `${props.plugin.name} has no storage.`,
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : `Could not set ${props.plugin.name}'s storage.`);
    } finally {
      setSaving(false);
    }
  };

  // Removing asks first; a refusal shows in that dialog.
  const [removing, setRemoving] = createSignal(false);
  const remove = async () => {
    const s = current();
    if (!s) return;
    setError(null);
    await deletePluginStorage(props.org, props.plugin.name);
    props.onChanged(`${props.plugin.name} no longer mounts ${s.claim}; the claim is left as it is.`);
  };

  return (
    <div class="plg-conns">
      <div class="token-form">
        <Field
          label="Claim"
          help={
            <>
              A PersistentVolumeClaim in the plugin namespace. An operator creates it with the
              annotation{" "}
              <span class="mono">
                {current()?.owner_annotation ?? "scarab.io/plugin-installation"}={owner()}
              </span>
              ; until it has that annotation the plugin does not start.
            </>
          }
          error={built().error && form().claim.trim() !== "" ? built().error : null}
        >
          <Input mono placeholder="argocd-state" value={form().claim} onValue={(v) => set("claim", v)} />
        </Field>
        <Field label="Mount path">
          <Input
            mono
            placeholder={DEFAULT_STORAGE_MOUNT_PATH}
            value={form().mount_path}
            onValue={(v) => set("mount_path", v)}
          />
        </Field>
        <p class="f-help">
          <Icon icon="shield-check" size={12} /> {STORAGE_NOTE} Moving it to another claim or path
          asks you to confirm with your passkey.
        </p>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
        <div class="modal-actions">
          <button class="btn btn-primary btn-sm" disabled={!built().body || saving()} onClick={() => void save()}>
            {saving() ? "Saving…" : "Save"}
          </button>
          <Show when={current()}>
            <button class="btn btn-ghost btn-sm" onClick={() => setRemoving(true)}>
              Remove
            </button>
          </Show>
        </div>
      </div>
      <Show when={removing() && current()}>
        {(s) => (
          <ConfirmAction
            eyebrow={`${props.org} · plugins`}
            title={`Stop mounting ${s().claim} in ${props.plugin.name}?`}
            purpose="The plugin restarts without it."
            confirmLabel="Stop mounting"
            onConfirm={remove}
            onClose={() => setRemoving(false)}
          >
            <p>The claim and its data stay in the cluster; Scarab does not delete them.</p>
          </ConfirmAction>
        )}
      </Show>
    </div>
  );
}

/** An installation's broker journal: the requests its plugin sent to the
 *  broker, newest first, a page at a time. */
function BrokerJournal(props: { org: string; installation: string }) {
  const [filter, setFilter] = createSignal<JournalFilter>({ phase: "", deniedOnly: false });
  const [entries, setEntries] = createSignal<BrokerJournalEntry[]>([]);
  const [nextBefore, setNextBefore] = createSignal<number | null>(null);
  const [loading, setLoading] = createSignal(false);
  const [loaded, setLoaded] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  // Each load is numbered; a page that comes back after the filter changed
  // is dropped.
  let generation = 0;

  const load = async (before: number | null) => {
    const mine = before == null ? ++generation : generation;
    setLoading(true);
    setError(null);
    try {
      const page = await getPluginBrokerJournal(props.org, props.installation, journalParams(filter(), before));
      if (mine !== generation) return;
      setEntries((shown) => (before == null ? page.entries : appendJournalPage(shown, page.entries)));
      setNextBefore(page.next_before ?? null);
      setLoaded(true);
    } catch (e) {
      if (mine !== generation) return;
      setError(e instanceof Error ? e.message : "Could not load the broker journal.");
    } finally {
      if (mine === generation) setLoading(false);
    }
  };

  createEffect(
    on(
      () => [props.org, props.installation, filter()] as const,
      () => void load(null),
    ),
  );

  return (
    <div class="plg-conns">
      <div class="secrets-actions sec-filter evlog-filter">
        <Field label="Phase">
          <Select
            value={filter().phase}
            onValue={(v) => setFilter((f) => ({ ...f, phase: v as JournalPhaseFilter }))}
          >
            <option value="">All phases</option>
            <option value="refused">refused</option>
            <option value="head">head</option>
            <option value="end">end</option>
          </Select>
        </Field>
        <Checkbox
          checked={filter().deniedOnly}
          onChecked={(v) => setFilter((f) => ({ ...f, deniedOnly: v }))}
          label="Refused or cut short only"
        />
      </div>
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
      <Show when={loaded()} fallback={<Show when={!error()}><p class="empty">loading…</p></Show>}>
        <Show when={entries().length > 0} fallback={<p class="empty">{journalEmptyText(filter())}</p>}>
          <div class="plg-journal-wrap">
          <table class="table sec-table plg-journal">
            <thead>
              <tr>
                <th>Time</th>
                <th>Phase</th>
                <th>Method</th>
                <th>Path</th>
                <th>Rule</th>
                <th>Status</th>
                <th>Bytes</th>
                <th>Duration</th>
                <th>Reason</th>
              </tr>
            </thead>
            <tbody>
              <For each={entries()}>
                {(e) => {
                  const v = journalEntryView(e);
                  return (
                    <tr title={`request ${e.request_id} · slot ${e.slot}`}>
                      <td class="mono" title={absTime(e.started_at)}>
                        {relTime(e.started_at)}
                      </td>
                      <td>
                        <span class={BADGE[v.tone]}>{v.phase}</span>
                      </td>
                      <td class="mono">{e.method}</td>
                      <td class="mono plg-journal-path">{v.path}</td>
                      <td class="mono plg-journal-rule">{v.rule}</td>
                      <td class="mono">{v.status}</td>
                      <td class="mono">{v.bytes}</td>
                      <td class="mono">{v.duration}</td>
                      <td class="mono plg-killed">{v.reason}</td>
                    </tr>
                  );
                }}
              </For>
            </tbody>
          </table>
          </div>
        </Show>
        <Show when={nextBefore()}>
          {(before) => (
            <div class="secrets-actions">
              <button class="btn btn-ghost btn-sm" disabled={loading()} onClick={() => void load(before())}>
                {loading() ? "Loading…" : "Load older"}
              </button>
            </div>
          )}
        </Show>
      </Show>
      <p class="subtle">
        <small>
          Kept for 14 days. Query values are not shown, only the parameter names.
        </small>
      </p>
    </div>
  );
}

/** An installation's connection slots: where the broker sends each one's
 *  reads, and whether its credential is set. */
function Connections(props: { org: string; installation: string }) {
  const [epoch, setEpoch] = createSignal(0);
  const [connections] = createResource(
    () => ({ org: props.org, name: props.installation, epoch: epoch() }),
    (k) => listPluginConnections(k.org, k.name),
  );
  const [error, setError] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const changed = (msg: string) => {
    setError(null);
    setNotice(msg);
    setEpoch((e) => e + 1);
  };

  return (
    <div class="plg-conns">
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
      <Show when={notice()}>
        <p class="notice-ok">{notice()}</p>
      </Show>
      <Show when={!connections.loading} fallback={<p class="empty">loading…</p>}>
        <Show when={!connections.error} fallback={<p class="error">Could not load the connections.</p>}>
          <For each={connections() ?? []} fallback={<p class="empty">This plugin declares no connections.</p>}>
            {(c) => (
              <ConnectionRow
                org={props.org}
                installation={props.installation}
                connection={c}
                onChanged={changed}
                onError={(msg) => {
                  setNotice(null);
                  setError(msg);
                }}
              />
            )}
          </For>
          <p class="subtle">
            <small>
              <Icon icon="shield-check" size={12} /> The plugin never holds the credential: the
              broker adds it to the plugin's reads and sends them to this destination only — the
              plugin cannot choose another host.
            </small>
          </p>
        </Show>
      </Show>
    </div>
  );
}

/** One connection slot: its binding, and the form that replaces it. */
function ConnectionRow(props: {
  org: string;
  installation: string;
  connection: PluginConnection;
  onChanged: (msg: string) => void;
  onError: (msg: string) => void;
}) {
  const c = () => props.connection;
  const badge = () => credentialBadge(c());
  const [editing, setEditing] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  // The credential is never prefilled: the server cannot give it back.
  const blank = (): ConnectionForm => ({ base_url: c().base_url ?? "", credential: "", ca_pem: c().ca_pem ?? "" });
  const [form, setForm] = createSignal<ConnectionForm>(blank());
  const set = (k: keyof ConnectionForm, v: string) => setForm((f) => ({ ...f, [k]: v }));
  const built = () => toConnectionBody(form());
  const urlProblem = () => connectionBaseUrlError(form().base_url);

  const save = async () => {
    const body = built().body;
    if (!body || saving()) return;
    setSaving(true);
    try {
      const bound = await putPluginConnection(props.org, props.installation, c().slot, body);
      setEditing(false);
      props.onChanged(`${c().slot} now goes to ${bound.base_url}.`);
    } catch (e) {
      props.onError(e instanceof Error ? e.message : `Could not bind ${c().slot}.`);
    } finally {
      setSaving(false);
    }
  };

  // Unbinding asks first; a refusal shows in that dialog.
  const [unbinding, setUnbinding] = createSignal(false);
  const unbind = async () => {
    await deletePluginConnection(props.org, props.installation, c().slot);
    props.onChanged(`${c().slot} is unbound; its credential is deleted.`);
  };

  return (
    <div class="plg-conn-row">
      <span class="mono plg-slot">{c().slot}</span>
      <span class="mono" title="Every request the plugin sends on this connection goes here, and nowhere else.">
        {c().base_url ?? "no destination"}
      </span>
      <span class={BADGE[badge().tone]} title={badge().title}>
        {badge().label}
      </span>
      <Show when={c().ca_pem}>
        <span class="subtle" title="These CA certificates are the only trust anchors for this destination.">
          <Icon icon="shield-check" size={12} /> custom CA
        </span>
      </Show>
      <Show when={c().insecure_http}>
        <span class="plg-killed" title={INSECURE_HTTP_WARNING}>
          <Icon icon="alert-triangle" size={12} /> plain http
        </span>
      </Show>
      <span class="oenv-actions">
        <button
          class="btn btn-ghost btn-sm"
          onClick={() => {
            setForm(blank());
            setEditing((e) => !e);
          }}
        >
          {c().configured ? "rebind" : "bind"}
        </button>
        <Show when={c().configured}>
          <button class="btn btn-ghost btn-sm" onClick={() => setUnbinding(true)}>
            unbind
          </button>
        </Show>
      </span>
      <span class="subtle">
        <small>
          reads only:{" "}
          <span class="mono">{c().read_rules.map(ruleLabel).join(", ")}</span>
        </small>
      </span>

      <Show when={editing()}>
        <div class="token-form">
          <Field label="Base URL" error={urlProblem()}>
            <Input
              mono
              placeholder="https://argocd.example.com"
              value={form().base_url}
              onValue={(v) => set("base_url", v)}
            />
          </Field>
          <Show when={!urlProblem() && isInsecureHttp(form().base_url)}>
            <p class="plg-killed">
              <Icon icon="alert-triangle" size={12} /> {INSECURE_HTTP_WARNING}
            </p>
          </Show>
          <Field
            label="Credential"
            help={
              <>
                Sent upstream as <span class="mono">Authorization: Bearer …</span>. Write-only: it is
                never shown again, and every save needs it — a new destination never keeps a
                credential entered for another.
              </>
            }
          >
            <Input
              mono
              type="password"
              autocomplete="new-password"
              placeholder={c().credential_set ? "set — enter it again to save" : "bearer token"}
              value={form().credential}
              onValue={(v) => set("credential", v)}
            />
          </Field>
          <Field
            label="CA (optional)"
            help="For an internal CA: these certificates replace the platform's roots for this destination. https only; there is no way to skip verification."
          >
            <Textarea
              mono
              rows={3}
              placeholder="-----BEGIN CERTIFICATE-----"
              value={form().ca_pem}
              onValue={(v) => set("ca_pem", v)}
            />
          </Field>
          <Show when={built().error && form().base_url.trim() !== ""}>
            <p class="error">{built().error}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary btn-sm" disabled={!built().body || saving()} onClick={() => void save()}>
              {saving() ? "Saving…" : "Save"}
            </button>
            <button class="btn btn-ghost btn-sm" onClick={() => setEditing(false)}>
              Cancel
            </button>
          </div>
        </div>
      </Show>
      <Show when={unbinding()}>
        <ConfirmDestructive
          eyebrow={`${props.org} · ${props.installation}`}
          title={`Unbind ${c().slot}?`}
          purpose="Its credential is deleted, and the plugin's requests on it are refused from now on."
          name={c().slot}
          confirmLabel="Unbind connection"
          onConfirm={unbind}
          onClose={() => setUnbinding(false)}
        >
          <p>A stream it has open is cut within seconds.</p>
        </ConfirmDestructive>
      </Show>
    </div>
  );
}

/** Install a plugin: read its manifest from a git source, review what it
 *  asks for, then approve it with an image pinned by digest. Every plugin
 *  takes this one path. */
function InstallDialog(props: {
  org: string;
  taken: string[];
  onClose: () => void;
  onInstalled: (p: PluginInstallation) => void;
}) {
  // A private repository is read through a connection of this org.
  const [connections] = createResource(
    () => props.org,
    (o) => listConnections(o).catch(() => []),
  );
  const [source, setSource] = createSignal<ManifestFetchForm>({ source: "", connection: "" });
  const [fetched, setFetched] = createSignal<FetchedPluginManifest | null>(null);
  // The connection the shown manifest was read through: the install names
  // the same one.
  const [fetchedVia, setFetchedVia] = createSignal("");
  const [name, setName] = createSignal("");
  const [image, setImage] = createSignal("");
  const [config, setConfig] = createSignal<Record<string, string>>({});
  const [storage, setStorage] = createSignal<StorageForm>({ claim: "", mount_path: "" });
  const [busy, setBusy] = createSignal<"" | "fetch" | "install">("");
  const [error, setError] = createSignal<string | null>(null);

  // Changing the source drops the review: what is installed is what was read.
  const setSourceField = (k: keyof ManifestFetchForm, v: string) => {
    setFetched(null);
    setSource((f) => ({ ...f, [k]: v }));
  };
  const fetchBody = () => toManifestFetchBody(source());
  const review = () => {
    const f = fetched();
    return f ? manifestReview(f) : null;
  };

  const doFetch = async () => {
    const body = fetchBody().body;
    if (!body || busy()) return;
    setBusy("fetch");
    setError(null);
    try {
      const f = await fetchPluginManifest(props.org, body);
      setFetched(f);
      setFetchedVia(body.connection ?? "");
      setConfig({});
      if (!name()) setName(suggestedInstallName(f.manifest.plugin, props.taken));
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not read the manifest.");
    } finally {
      setBusy("");
    }
  };

  const built = () => {
    const f = fetched();
    return toInstallBody(
      { name: name(), image: image(), config: config(), storage: storage() },
      f ? { source: f.source, manifest_sha256: f.manifest_sha256, connection: fetchedVia() || undefined } : null,
      { taken: props.taken },
    );
  };
  // Quiet while a field is untouched.
  const nameProblem = () => (name() ? installNameError(name(), props.taken) : null);
  const imageProblem = () => imageError(image().trim());
  const storageProblem = () => {
    const s = storage();
    if (s.claim.trim() === "" && s.mount_path.trim() === "") return null;
    return toStorageBody(s).error ?? null;
  };

  const install = async () => {
    const body = built().body;
    if (!body || busy()) return;
    setBusy("install");
    setError(null);
    try {
      props.onInstalled(await installPlugin(props.org, body));
    } catch (e) {
      // A 409 (name taken or retired, or the source no longer reads as the
      // reviewed manifest) is the whole answer.
      setError(e instanceof Error ? e.message : "Could not install the plugin.");
    } finally {
      setBusy("");
    }
  };

  return (
    <Modal
      size="wide"
      eyebrow={`${props.org} · plugins`}
      title="Install plugin"
      purpose="Read the plugin's manifest, review what it asks for, and approve it with one image digest."
      busy={busy() !== ""}
      onClose={props.onClose}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={busy() !== ""}>
            Cancel
          </button>
          <button
            class="btn btn-primary"
            type="button"
            disabled={!built().body || busy() !== ""}
            onClick={() => void install()}
          >
            <Icon icon="plus" size={14} /> {busy() === "install" ? "Installing…" : "Install"}
          </button>
        </>
      }
    >
      <ModalSection n={1} title="Manifest source">
        <Field
          label="Source"
          required
          error={manifestSourceError(source().source)}
          help={
            <>
              The plugin's manifest file in a git repository at a full commit id:{" "}
              <span class="mono">&lt;host&gt;/&lt;owner&gt;/&lt;repo&gt;//&lt;path&gt;@&lt;commit&gt;</span>
              , the path ending in <span class="mono">.json</span>, <span class="mono">.yaml</span> or{" "}
              <span class="mono">.yml</span>. A public repository on github.com or a Forgejo
              connection's host is read without credentials.
            </>
          }
        >
          <Input
            mono
            placeholder="github.com/acme/scarab-plugin-argocd//plugin.json@<commit>"
            value={source().source}
            onValue={(v) => setSourceField("source", v)}
          />
        </Field>
        <Field label="Connection (private repository)">
          <Select value={source().connection} onValue={(v) => setSourceField("connection", v)}>
            <option value="">none: read it anonymously</option>
            <For each={connections() ?? []}>
              {(c) => (
                <option value={c.id}>
                  {c.id} ({c.kind}, {c.base_url})
                </option>
              )}
            </For>
          </Select>
        </Field>
        <div class="modal-actions">
          <button
            class="btn btn-ghost btn-sm"
            type="button"
            disabled={!fetchBody().body || busy() !== ""}
            onClick={() => void doFetch()}
          >
            <Icon icon="search" size={14} /> {busy() === "fetch" ? "Reading…" : "Read and review"}
          </button>
        </div>
        <p class="f-help">Reading stores nothing; nothing is installed until you approve it below.</p>
      </ModalSection>

      <Show when={fetched() && review()}>
        <ModalSection n={2} title="What it asks for">
          <ManifestReviewView fetched={fetched()!} />
        </ModalSection>

        <ModalSection n={3} title="Installation">
          <Field
            label="Name"
            required
            error={nameProblem()}
            help={
              <>
                A DNS-1123 label: lowercase letters, digits and "-", at most 63. It acts as{" "}
                <span class="mono">plugin:{name() || "<name>"}</span>. Unique in the org and never
                reused — not even after an uninstall.
              </>
            }
          >
            <Input mono placeholder={review()!.plugin} value={name()} onValue={setName} />
          </Field>
          <Field
            label="Image"
            required
            error={imageProblem()}
            help={
              <>
                Any repository, pinned by digest: <span class="mono">&lt;repository&gt;@sha256:&lt;64 hex&gt;</span>.
                A tag can change under the same name; a digest cannot.
              </>
            }
          >
            <Input mono placeholder="registry.example.com/team/plugin@sha256:…" value={image()} onValue={setImage} />
          </Field>
        </ModalSection>

        <Show when={review()!.configKeys.length > 0}>
          <ModalSection n={4} title="Settings">
            <For each={review()!.configKeys}>
              {(key) => (
                <Field plainLabel label={<span class="mono">{key}</span>}>
                  <Input
                    mono
                    placeholder="optional"
                    value={config()[key] ?? ""}
                    onValue={(v) => setConfig((cfg) => ({ ...cfg, [key]: v }))}
                  />
                </Field>
              )}
            </For>
          </ModalSection>
        </Show>

        <ModalSection n={review()!.configKeys.length > 0 ? 5 : 4} title="Storage (optional)">
          <Field
            label="Claim"
            error={storageProblem()}
            help={
              <>
                A PersistentVolumeClaim an operator created in the plugin namespace, with the
                annotation{" "}
                <span class="mono">
                  scarab.io/plugin-installation={props.org}/{name() || "<name>"}
                </span>
                . Leave it blank for none; you can add it later.
              </>
            }
          >
            <Input
              mono
              placeholder="argocd-state"
              value={storage().claim}
              onValue={(v) => setStorage((s) => ({ ...s, claim: v }))}
            />
          </Field>
          <Field label="Mount path">
            <Input
              mono
              placeholder={DEFAULT_STORAGE_MOUNT_PATH}
              value={storage().mount_path}
              onValue={(v) => setStorage((s) => ({ ...s, mount_path: v }))}
            />
          </Field>
        </ModalSection>
      </Show>
    </Modal>
  );
}

/** Everything a fetched manifest asks for, as the install dialog shows it
 *  before approval. */
function ManifestReviewView(props: { fetched: FetchedPluginManifest }) {
  const r = () => manifestReview(props.fetched);
  return (
    <div class="plg-review">
      <dl class="lib-facts">
        <dt>Plugin</dt>
        <dd class="m">{r().plugin}</dd>
        <dt>Source</dt>
        <dd class="m">{props.fetched.source}</dd>
        <dt>Manifest SHA-256</dt>
        <dd class="m">{props.fetched.manifest_sha256}</dd>
        <dt>Capabilities</dt>
        <dd>
          <Show when={r().capabilities.length > 0} fallback={<span class="subtle">none</span>}>
            <ul class="plg-review-list">
              <For each={r().capabilities}>
                {(c) => (
                  <li>
                    <span class="mono">{c.key}</span> <span class="subtle">· {c.label}</span>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </dd>
        <dt>Connections</dt>
        <dd>
          <Show when={r().connections.length > 0} fallback={<span class="subtle">none</span>}>
            <ul class="plg-review-list">
              <For each={r().connections}>
                {(c) => (
                  <li>
                    <span class="mono">{c.name}</span> <span class="subtle">· auth {c.auth || "none"} · reads only:</span>
                    <ul class="plg-review-rules">
                      <For each={c.rules}>{(rule) => <li class="mono">{rule}</li>}</For>
                    </ul>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </dd>
        <dt>Config keys</dt>
        <dd class="m">
          <Show when={r().configKeys.length > 0} fallback={<span class="subtle">none</span>}>
            {r().configKeys.join(", ")}
          </Show>
        </dd>
        <dt>Gates</dt>
        <dd>
          <Show when={r().gates.length > 0} fallback={<span class="subtle">none</span>}>
            <ul class="plg-review-list">
              <For each={r().gates}>
                {(g) => (
                  <li>
                    <span class="mono">{g.name}</span>
                    <Show when={g.description}>
                      <span class="subtle"> · {g.description}</span>
                    </Show>
                    <div class="subtle">
                      inputs <span class="mono">{g.inputs.join(", ") || "none"}</span> · outputs{" "}
                      <span class="mono">{g.outputs.join(", ") || "none"}</span>
                    </div>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </dd>
        <dt>Installed here</dt>
        <dd>
          <Show when={r().installed.length > 0} fallback={<span class="subtle">no installation of this plugin</span>}>
            <ul class="plg-review-list">
              <For each={r().installed}>
                {(i) => (
                  <li>
                    <span class="mono">{i.name}</span>{" "}
                    <span class="subtle" title={i.image}>
                      · <span class="mono">{shortDigest(i.image)}</span> ·{" "}
                      {i.sameManifest ? "approved with this manifest" : "approved with a different manifest"}
                    </span>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </dd>
      </dl>
      <details class="lib-errs">
        <summary>the file as read · {props.fetched.content.split("\n").length} lines</summary>
        <pre>{props.fetched.content}</pre>
      </details>
    </div>
  );
}
