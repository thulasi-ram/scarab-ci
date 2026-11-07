// Plugins — the Settings section where an org admin installs a plugin, sees
// whether it is running, kills it, and binds its connections.
//
// A PluginInstallation is the approval of exactly (image digest, manifest), so
// the install form asks for a digest and shows what the manifest asks for.
// Every catalog plugin installs through the same form; its publisher is shown
// as a field, read from the image repository. A refusal from the server is
// shown verbatim. Kill revokes at the token-to-installation mapping, so it
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
// "Storage" names one PersistentVolumeClaim an operator created
// in the plugin namespace; the runtime mounts it read-write into the plugin's
// Pod. Scarab does not create, back up or read it. Moving it to another claim
// or path asks for a step-up; adding or removing it does not.
import { createResource, createSignal, For, Show } from "solid-js";
import {
  deletePluginConnection,
  deletePluginStorage,
  enablePlugin,
  installPlugin,
  killPlugin,
  listPluginCatalog,
  listPluginConnections,
  listPlugins,
  putPluginConnection,
  putPluginStorage,
  uninstallPlugin,
  type PluginCatalogEntry,
  type PluginConnection,
  type PluginInstallation,
} from "../api/client";
import {
  connectionBaseUrlError,
  credentialBadge,
  imageError,
  imageRepository,
  INSECURE_HTTP_WARNING,
  isInsecureHttp,
  pluginPublisher,
  toConnectionBody,
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
import { Field, Input, Select, Textarea } from "./Field";

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
                  No plugins installed. A plugin connects Scarab to a vendor tool, such as
                  Argo CD.
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
                <span class="mono">plugin:&lt;name&gt;</span>, never as a person.
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

  return (
    <li class="tok" classList={{ "tok-dead": killed() }}>
      <div class="tok-head">
        <Icon icon="boxes" size={15} />
        <span class="tok-name mono">{p().name}</span>
        <span class="tok-scope mono" title="The catalog plugin">
          {p().plugin}
        </span>
        <Show when={pluginPublisher(p().image)}>
          {(pub) => (
            <span class="tok-scope" title="The publisher: the owner of the image repository">
              by <span class="mono">{pub()}</span>
            </span>
          )}
        </Show>
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

/** Install a catalog plugin, pinned by digest. Every catalog entry takes
 *  this one path; the publisher is shown, not branched on. */
function InstallDialog(props: {
  org: string;
  taken: string[];
  onClose: () => void;
  onInstalled: (p: PluginInstallation) => void;
}) {
  const [catalog] = createResource(
    () => props.org,
    (o) => listPluginCatalog(o),
  );
  const [plugin, setPlugin] = createSignal("");
  const [name, setName] = createSignal("");
  const [image, setImage] = createSignal("");
  const [config, setConfig] = createSignal<Record<string, string>>({});
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const entries = () => catalog() ?? [];
  const entry = (): PluginCatalogEntry | undefined => entries().find((c) => c.plugin === plugin());

  /** Picking a plugin prefills its repository (the admin supplies the
   *  digest) and, if the name is still blank, its catalog key. */
  const choose = (key: string) => {
    setPlugin(key);
    const c = entries().find((x) => x.plugin === key);
    setConfig({});
    if (!c) return;
    setImage(`${c.image_repository}@sha256:`);
    if (!name()) setName(c.plugin);
  };

  const built = () =>
    toInstallBody(
      { plugin: plugin(), name: name(), image: image(), config: config() },
      { repository: entry()?.image_repository, taken: props.taken },
    );
  // Quiet while a field is untouched: a blank name, or the prefilled
  // `<repository>@sha256:` still waiting for its digest.
  const nameProblem = () => (name() ? installNameError(name(), props.taken) : null);
  const imageProblem = () => {
    const img = image().trim();
    const untouched = !!entry() && img === `${entry()!.image_repository}@sha256:`;
    return untouched ? null : imageError(img, entry()?.image_repository);
  };

  const install = async () => {
    const body = built().body;
    if (!body || saving()) return;
    setSaving(true);
    setError(null);
    try {
      props.onInstalled(await installPlugin(props.org, body));
    } catch (e) {
      // A 403 (the server refuses this plugin) or a 409 (name taken or
      // retired, or plugins off) is the whole answer.
      setError(e instanceof Error ? e.message : "Could not install the plugin.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      eyebrow={`${props.org} · plugins`}
      title="Install plugin"
      purpose="Approve one image digest and exactly what its manifest asks for."
      busy={saving()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={saving()}>
            Cancel
          </button>
          <button class="btn btn-primary" type="button" disabled={!built().body || saving()} onClick={install}>
            <Icon icon="plus" size={14} /> {saving() ? "Installing…" : "Install"}
          </button>
        </>
      }
    >
      <ModalSection n={1} title="Plugin">
        <Show when={catalog.error}>
          <p class="error">Could not load the plugin catalog.</p>
        </Show>
        <Show when={!catalog.loading && !catalog.error && entries().length === 0}>
          <p class="empty">This server has no plugins to install.</p>
        </Show>
        <Field label="Plugin" required help={entry()?.description}>
          <Select mono value={plugin()} onValue={choose}>
            <option value="">choose a plugin…</option>
            <For each={entries()}>
              {(c) => (
                <option value={c.plugin}>
                  {pluginPublisher(c.image_repository) ? `${c.plugin} · ${pluginPublisher(c.image_repository)}` : c.plugin}
                </option>
              )}
            </For>
          </Select>
        </Field>
        <Show when={entry()}>
          {(c) => (
            <div>
              <p class="dlg-ctx">
                <span class="k">publisher</span>{" "}
                {pluginPublisher(c().image_repository) || "none in the image name"}
              </p>
              <p class="f-help">
                The owner of <span class="mono">{c().image_repository}</span>: whose image you are
                approving.
              </p>
            </div>
          )}
        </Show>
      </ModalSection>

      <ModalSection n={2} title="Installation">
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
          <Input mono placeholder="argocd-prod" value={name()} onValue={setName} />
        </Field>
        <Field
          label="Image"
          required
          error={imageProblem()}
          help={
            <>
              Pinned by digest — paste the <span class="mono">sha256:</span> of the release you are
              approving. A tag can change under the same name; a digest cannot.
            </>
          }
        >
          <Input mono placeholder="ghcr.io/…@sha256:…" value={image()} onValue={setImage} />
        </Field>
      </ModalSection>

      <Show when={entry()}>
        {(c) => (
          <>
            <Show when={c().manifest.config_keys.length > 0}>
              <ModalSection n={3} title="Settings">
                <For each={c().manifest.config_keys}>
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
            <ModalSection n={c().manifest.config_keys.length > 0 ? 4 : 3} title="You are approving">
              <div class="plg-caps">
                <For each={c().manifest.capabilities}>
                  {(cap) => <span class="dep-tag dep-tag-muted">{cap.replace(/_/g, " ")}</span>}
                </For>
              </div>
              <For each={c().manifest.connections}>
                {(slot) => (
                  <p class="f-help">
                    connection <span class="mono">{slot.name}</span> ({slot.auth_schemes.join(", ")}) —
                    read only: <span class="mono">{slot.read_rules.map(ruleLabel).join(", ")}</span>
                  </p>
                )}
              </For>
            </ModalSection>
          </>
        )}
      </Show>
    </Modal>
  );
}
