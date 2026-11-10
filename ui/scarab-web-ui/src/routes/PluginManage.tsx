// One plugin installation's page, `/orgs/<org>/settings/plugins/<id>/manage`,
// inside the settings frame with Plugins marked in the sidebar.
//
// It holds everything about the installation: what it is and what it was
// approved with (image digest, manifest, capabilities, config, gates),
// whether it runs, Kill and Enable, its storage claim, its connections, its
// broker journal, and Uninstall, which asks for the name to be typed.
//
// Kill revokes at the token-to-installation mapping, so it bites on the next
// request rather than when a token expires. Uninstall retires the name
// forever.
//
// "Connections" bind each manifest connection slot to the ONE destination
// the CredentialBroker sends the plugin's reads to, and the credential it
// adds. The credential is write-only (the page only says "set" or "not set")
// and is typed again on every save, so a destination never inherits a
// credential entered for another. Plain http is allowed but always flagged.
//
// "Broker journal" lists the requests the plugin sent to the broker in the
// last 14 days, newest first: what was forwarded, what was refused and why.
// Query values are never shown, only the parameter names.
//
// "Storage" names one PersistentVolumeClaim an operator created in the
// plugin namespace; the runtime mounts it read-write into the plugin's Pod.
// Scarab does not create, back up or read it. Moving it to another claim or
// path asks for a step-up; adding or removing it does not.
//
// "Upgrade" reads a new manifest (by default the recorded source's file at a
// new commit), shows what it adds, removes and keeps against the installed
// one, and applies it with a new image. The installation keeps its name, id,
// config, storage and connections. The server decides whether it needs a
// passkey: a new image digest is new code and always does, as an install
// does, and so does a manifest that asks for more; only a narrower manifest
// on the installed image needs none.
import { createEffect, createMemo, createResource, createSignal, For, on, Show } from "solid-js";
import { A, useNavigate, useParams } from "@solidjs/router";
import {
  deletePluginConnection,
  deletePluginStorage,
  enablePlugin,
  fetchPluginUpgrade,
  getPluginBrokerJournal,
  getPluginInstallation,
  killPlugin,
  listPluginConnections,
  putPluginConnection,
  putPluginStorage,
  uninstallPlugin,
  upgradePlugin,
  type BrokerJournalEntry,
  type FetchedPluginUpgrade,
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
  INSECURE_HTTP_WARNING,
  isInsecureHttp,
  pluginChips,
  toConnectionBody,
  type ConnectionForm,
  runtimeBadge,
  shortDigest,
  ruleLabel,
  DEFAULT_STORAGE_MOUNT_PATH,
  STORAGE_NOTE,
  toStorageBody,
  type StorageForm,
  imageChangesTheAnswer,
  imageError,
  imageRepository,
  imageRepositoryError,
  manifestImageFact,
  sourceLabel,
  toUpgradeBody,
  toUpgradeFetchBody,
  upgradeDiffCounts,
  upgradeDiffView,
  upgradeStepUpLine,
  type DiffGroupView,
  type UpgradeFetchForm,
} from "../plugins";
import { absTime, relTime } from "../fmt";
import { decodeParam, orgHref } from "../settings-nav";
import { SettingsCrumbs, SettingsFrame } from "./Settings";
import { Section } from "./LibraryManage";
import { BADGE } from "../components/Plugins";
import Chips from "../components/Chips";
import Icon from "../components/Icon";
import ConfirmDestructive, { ConfirmAction } from "../components/ConfirmDestructive";
import { Checkbox, Field, Input, Select, Textarea } from "../components/Field";
import Modal, { ModalMessage, ModalSection } from "../components/Modal";

/** `/orgs/:org/settings/plugins/:id/manage`. */
export default function PluginManage() {
  const params = useParams<{ org: string; id: string }>();
  const org = () => decodeParam(params.org);
  const id = () => decodeParam(params.id);
  const key = createMemo(() => JSON.stringify([org(), id()]));
  return (
    <SettingsFrame kind="org" org={org()} section="plugins">
      {/* Keyed: another installation in the URL is a new page. */}
      <Show when={key()} keyed>
        {(_key) => <Manage org={org()} id={id()} />}
      </Show>
    </SettingsFrame>
  );
}

function Manage(props: { org: string; id: string }) {
  const nav = useNavigate();
  const back = () => orgHref(props.org, "plugins");
  const [installation, { refetch }] = createResource(() => getPluginInstallation(props.org, props.id));
  const plugin = () => (installation.error ? undefined : (installation() ?? undefined));
  const [notice, setNotice] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const done = (msg: string) => {
    setError(null);
    setNotice(msg);
    void refetch();
  };

  return (
    <>
      <SettingsCrumbs
        trail={[{ label: "Plugins", href: back() }, { label: plugin()?.name ?? props.id, mono: true }]}
        sub={
          <>
            <span class="mono">{props.org}</span> · manage a plugin installation
          </>
        }
      />
      <Show when={notice()}>
        <p class="notice-ok">{notice()}</p>
      </Show>
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
      <Show when={!installation.error} fallback={<p class="error">Could not load the plugin installation.</p>}>
        <Show when={!installation.loading || plugin()} fallback={<p class="empty">loading…</p>}>
          <Show
            when={plugin()}
            fallback={
              <p class="empty">
                There is no plugin installation <b class="mono">{props.id}</b> in {props.org}.{" "}
                <A href={back()}>All plugins</A>
              </p>
            }
          >
            {(p) => (
              <Installation
                org={props.org}
                plugin={p()}
                onDone={done}
                onError={(m) => {
                  setNotice(null);
                  setError(m);
                }}
                onUninstalled={() => nav(back())}
              />
            )}
          </Show>
        </Show>
      </Show>
    </>
  );
}

function Installation(props: {
  org: string;
  plugin: PluginInstallation;
  onDone: (msg: string) => void;
  onError: (msg: string) => void;
  onUninstalled: () => void;
}) {
  const p = () => props.plugin;
  const killed = () => p().state === "killed";
  const runtime = () => runtimeBadge(p().runtime_status);
  const [killing, setKilling] = createSignal(false);
  const [upgrading, setUpgrading] = createSignal(false);
  const [uninstalling, setUninstalling] = createSignal(false);
  const configKeys = () => [...new Set([...p().manifest.config_keys, ...Object.keys(p().config)])];

  const enable = async () => {
    try {
      await enablePlugin(props.org, p().name);
      props.onDone(`${p().name} is enabled again.`);
    } catch (e) {
      props.onError(e instanceof Error ? e.message : `Could not enable ${p().name}.`);
    }
  };

  return (
    <>
      <div class="run-toolbar">
        <button class="btn btn-ghost btn-sm" onClick={() => setUpgrading(true)}>
          <Icon icon="arrow-up" size={14} /> Upgrade
        </button>
        <Show
          when={killed()}
          fallback={
            <button class="btn btn-danger btn-sm" onClick={() => setKilling(true)}>
              <Icon icon="ban" size={14} /> Kill
            </button>
          }
        >
          <button class="btn btn-primary btn-sm" onClick={() => void enable()}>
            Enable
          </button>
        </Show>
      </div>

      <Section title="Installation" aside={<Chips chips={pluginChips(p())} />}>
        <dl class="kv">
          <dt>Name</dt>
          <dd class="m">{p().name}</dd>
          <dt>Plugin</dt>
          <dd class="m">{p().plugin}</dd>
          <dt>Acts as</dt>
          <dd class="m" title="What it acts as in audit, not tied to any person">
            {p().principal}
          </dd>
          <dt>Image</dt>
          <dd class="m" title={p().image}>
            {p().image}
          </dd>
          <dt>Manifest SHA-256</dt>
          <dd class="m">{p().manifest_sha256}</dd>
          <dt>Source</dt>
          <dd class="m" classList={{ subtle: !p().source }} title="Where the approved manifest was read">
            {sourceLabel(p())}
            <Show when={p().source_connection}>
              {(c) => <span class="subtle"> · read through {c()}</span>}
            </Show>
          </dd>
          <dt>Named image</dt>
          <dd class={p().manifest.image ? "m" : "plg-warn"}>
            {p().manifest.image ?? "The manifest does not name its image."}
          </dd>
          <dt>Approved</dt>
          <dd title={absTime(p().approved_at)}>
            by <span class="mono">{p().approved_by}</span> · {relTime(p().approved_at)}
          </dd>
          <Show when={killed() && p().killed_at}>
            <dt>Killed</dt>
            <dd class="plg-killed" title={absTime(p().killed_at!)}>
              by {p().killed_by ?? "?"} · {relTime(p().killed_at!)}
            </dd>
          </Show>
          <dt>Runtime</dt>
          <dd title={runtime().title}>
            <span class={BADGE[runtime().tone]}>{runtime().label}</span>
            <Show when={p().runtime_message}>
              <span class="plg-msg"> {p().runtime_message}</span>
            </Show>
          </dd>
          <dt>Isolation probe</dt>
          <dd>
            <Show when={p().probe_passed_at} fallback={<span class="subtle">not passed yet</span>}>
              {(at) => <span title={absTime(at())}>passed {relTime(at())}</span>}
            </Show>
          </dd>
          <dt>Capabilities</dt>
          <dd class="m">{p().manifest.capabilities.join(", ") || "none"}</dd>
        </dl>
      </Section>

      <Section title="Config">
        <Show
          when={configKeys().length > 0}
          fallback={<p class="kv-note kv-lead">The manifest declares no config keys.</p>}
        >
          <dl class="kv">
            <For each={configKeys()}>
              {(k) => (
                <>
                  <dt class="mono">{k}</dt>
                  <dd class="m">{p().config[k] ?? <span class="subtle">not set</span>}</dd>
                </>
              )}
            </For>
          </dl>
          <p class="kv-note">Set at install. A new value is a new installation.</p>
        </Show>
      </Section>

      <Section title="Gates">
        <Show
          when={p().manifest.gates.length > 0}
          fallback={<p class="kv-note kv-lead">This plugin resolves no gates.</p>}
        >
          <dl class="kv">
            <For each={p().manifest.gates}>
              {(g) => (
                <>
                  <dt class="mono">{g.name}</dt>
                  <dd>
                    <Show when={g.description}>{g.description} </Show>
                    <span class="subtle">
                      inputs <span class="mono">{g.inputs.map((f) => f.name).join(", ") || "none"}</span> · outputs{" "}
                      <span class="mono">{g.outputs.map((f) => f.name).join(", ") || "none"}</span>
                    </span>
                  </dd>
                </>
              )}
            </For>
          </dl>
          <p class="kv-note">
            Its gates run only when an enforced org plugin policy names this installation.
          </p>
        </Show>
      </Section>

      <Section title="Storage">
        <StorageEditor org={props.org} plugin={p()} onChanged={props.onDone} />
      </Section>

      <Show when={p().manifest.connections.length > 0}>
        <Section title="Connections">
          <Connections org={props.org} installation={p().name} />
        </Section>
      </Show>

      <Section title="Broker journal">
        <BrokerJournal org={props.org} installation={p().name} />
      </Section>

      <Section
        title="Uninstall"
        danger
        foot={
          <button class="btn btn-danger btn-sm" onClick={() => setUninstalling(true)}>
            Uninstall plugin
          </button>
        }
      >
        <p class="kv-note kv-lead">The name {p().name} is retired: it can never be installed again in this org.</p>
      </Section>

      <Show when={upgrading()}>
        <UpgradeDialog
          org={props.org}
          plugin={p()}
          onClose={() => setUpgrading(false)}
          onUpgraded={(u) => {
            setUpgrading(false);
            props.onDone(`${u.name} is upgraded to ${shortDigest(u.image)}.`);
          }}
        />
      </Show>
      <Show when={killing()}>
        <ConfirmAction
          eyebrow={`${props.org} · plugins`}
          title={`Kill ${p().name}?`}
          purpose="Nothing is uninstalled: Enable brings it back."
          confirmLabel="Kill plugin"
          danger
          onConfirm={async () => {
            await killPlugin(props.org, p().name);
            props.onDone(`${p().name} is killed.`);
          }}
          onClose={() => setKilling(false)}
        >
          <p>
            This revokes it at once: from the next request on, its broker calls are refused, even
            with a token that has not expired.
          </p>
        </ConfirmAction>
      </Show>
      <Show when={uninstalling()}>
        <ConfirmDestructive
          eyebrow={`${props.org} · plugins`}
          title={`Uninstall ${p().name}?`}
          purpose={`The name ${p().name} is retired forever: it can never be installed again.`}
          name={p().name}
          confirmLabel="Uninstall plugin"
          onConfirm={async () => {
            await uninstallPlugin(props.org, p().name);
            props.onUninstalled();
          }}
          onClose={() => setUninstalling(false)}
        />
      </Show>
    </>
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
    <div class="plg-body">
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
    <div class="plg-body">
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
    <div class="plg-body">
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

/** Upgrade an installation: read the new manifest, review what it adds,
 *  removes and keeps against the installed one, and apply it with a new
 *  image. The server says whether it needs a passkey, and decides it again
 *  when the upgrade is sent. */
function UpgradeDialog(props: {
  org: string;
  plugin: PluginInstallation;
  onClose: () => void;
  onUpgraded: (p: PluginInstallation) => void;
}) {
  const p = () => props.plugin;
  const [form, setForm] = createSignal<UpgradeFetchForm>({ ref: "", connection: "", image: "" });
  const [fetched, setFetched] = createSignal<FetchedPluginUpgrade | null>(null);
  const [busy, setBusy] = createSignal<"" | "fetch" | "upgrade">("");
  const [error, setError] = createSignal<string | null>(null);
  // The image the server judged the upgrade with: the one sent with the
  // read, or the installed one.
  const [judgedImage, setJudgedImage] = createSignal("");
  // Another source or connection drops the review: what is applied is what
  // was compared. So does another image, since a new digest changes whether
  // a passkey is needed; a half-typed one does not yet.
  const set = (k: keyof UpgradeFetchForm, v: string) => {
    if (k !== "image" || imageChangesTheAnswer(v, judgedImage())) setFetched(null);
    setForm((f) => ({ ...f, [k]: v }));
  };
  const fetchBody = () => toUpgradeFetchBody(form(), p().source);
  const built = () => toUpgradeBody(form().image, fetched());
  const groups = () => {
    const f = fetched();
    return f ? upgradeDiffView(f.diff) : [];
  };
  const imageProblem = () =>
    imageError(form().image.trim()) ??
    imageRepositoryError(form().image, fetched()?.manifest.image ?? p().manifest.image);

  const doFetch = async () => {
    const body = fetchBody().body;
    if (!body || busy()) return;
    setBusy("fetch");
    setError(null);
    try {
      setFetched(await fetchPluginUpgrade(props.org, p().name, body));
      setJudgedImage(body.image ?? p().image);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not read the new manifest.");
    } finally {
      setBusy("");
    }
  };

  const apply = async () => {
    const body = built().body;
    if (!body || busy()) return;
    setBusy("upgrade");
    setError(null);
    try {
      props.onUpgraded(await upgradePlugin(props.org, p().name, body));
    } catch (e) {
      // A 409 (the installation or the source changed since the review) is
      // the whole answer.
      setError(e instanceof Error ? e.message : `Could not upgrade ${p().name}.`);
    } finally {
      setBusy("");
    }
  };

  return (
    <Modal
      size="wide"
      eyebrow={`${props.org} · ${p().name}`}
      title={`Upgrade ${p().name}`}
      purpose="Read the new manifest, review what changes, and run it with a new image."
      busy={busy() !== ""}
      onClose={props.onClose}
      after={{ label: "Kept", value: "the name, id, config, storage, state and connections" }}
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
            onClick={() => void apply()}
          >
            <Icon icon="arrow-up" size={14} /> {busy() === "upgrade" ? "Upgrading…" : "Upgrade"}
          </button>
        </>
      }
    >
      <ModalSection n={1} title="New version">
        <Field
          label={p().source ? "Commit or source" : "Source"}
          required
          error={fetchBody().error && form().ref.trim() !== "" ? fetchBody().error : null}
          help={
            <Show
              when={p().source}
              fallback={
                <>
                  This installation's source was not recorded. Enter the manifest's whole source:{" "}
                  <span class="mono">&lt;host&gt;/&lt;owner&gt;/&lt;repo&gt;//&lt;path&gt;@&lt;commit&gt;</span>.
                </>
              }
            >
              {(src) => (
                <>
                  A full commit id reads <span class="mono">{src().replace(/@[0-9a-f]+$/, "")}</span> at
                  that commit. A whole source reads another file.
                </>
              )}
            </Show>
          }
        >
          <Input mono placeholder={p().source ? "<40-character commit>" : "github.com/acme/plugin//manifest.json@<commit>"} value={form().ref} onValue={(v) => set("ref", v)} />
        </Field>
        <Field
          label="Image"
          required
          error={imageProblem()}
          help={
            <>
              Pinned by digest. Now <span class="mono">{imageRepository(p().image)}</span> at{" "}
              <span class="mono">{shortDigest(p().image)}</span>.
            </>
          }
        >
          <Input
            mono
            placeholder={`${imageRepository(p().image)}@sha256:…`}
            value={form().image}
            onValue={(v) => set("image", v)}
          />
        </Field>
        <div class="modal-actions">
          <button
            class="btn btn-ghost btn-sm"
            type="button"
            disabled={!fetchBody().body || busy() !== ""}
            onClick={() => void doFetch()}
          >
            <Icon icon="search" size={14} /> {busy() === "fetch" ? "Reading…" : "Read and compare"}
          </button>
        </div>
      </ModalSection>

      <Show when={fetched()}>
        {(f) => (
          <ModalSection n={2} title="What changes">
            <dl class="kv">
              <dt>Source</dt>
              <dd class="m">{f().source}</dd>
              <dt>Manifest SHA-256</dt>
              <dd class="m">{f().manifest_sha256}</dd>
              <dt>Named image</dt>
              <dd classList={{ m: !manifestImageFact(f().manifest).warn, "plg-warn": manifestImageFact(f().manifest).warn }}>
                {f().manifest.image ?? manifestImageFact(f().manifest).text}
              </dd>
            </dl>
            <UpgradeDiff groups={groups()} />
            <div classList={{ "f-help": true, "plg-warn": f().needs_step_up }} data-testid="upgrade-step-up">
              <p>
                <Icon icon={f().needs_step_up ? "alert-triangle" : "shield-check"} size={12} />{" "}
                {upgradeStepUpLine(f())}
              </p>
              <Show when={f().step_up_reasons.length > 0}>
                <ul class="plg-diff-list">
                  <For each={f().step_up_reasons}>{(r) => <li>{r}</li>}</For>
                </ul>
              </Show>
            </div>
          </ModalSection>
        )}
      </Show>
    </Modal>
  );
}

/** The diff of an upgrade, group by group: added in the warning colour,
 *  removed struck through, unchanged collapsed. */
function UpgradeDiff(props: { groups: DiffGroupView[] }) {
  const counts = () => upgradeDiffCounts(props.groups);
  return (
    <div class="plg-diff">
      <p class="subtle">
        {counts().added} added · {counts().removed} removed · {counts().unchanged} unchanged
      </p>
      <For each={props.groups}>
        {(g) => (
          <section class="plg-diff-group">
            <h4 class="plg-diff-h">{g.title}</h4>
            <ul class="plg-diff-list">
              <For each={g.added}>
                {(x) => (
                  <li class="plg-diff-add" title="Added">
                    + {x}
                  </li>
                )}
              </For>
              <For each={g.removed}>
                {(x) => (
                  <li class="plg-diff-del" title="Removed">
                    − {x}
                  </li>
                )}
              </For>
            </ul>
            <Show when={g.unchanged.length > 0}>
              <details>
                <summary class="subtle">{g.unchanged.length} unchanged</summary>
                <ul class="plg-diff-list plg-diff-same">
                  <For each={g.unchanged}>{(x) => <li>{x}</li>}</For>
                </ul>
              </details>
            </Show>
          </section>
        )}
      </For>
    </div>
  );
}
