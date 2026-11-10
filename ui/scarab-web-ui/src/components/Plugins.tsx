// Plugins: the Settings section listing an org's plugin installations, one
// row each, with its image digest, its state tags and a link to its own page
// (`…/settings/plugins/<id>/manage`, routes/PluginManage.tsx), where it is
// killed, enabled, given storage and connections, and uninstalled.
//
// A PluginInstallation is the approval of exactly (image digest, manifest).
// Every plugin installs the same way: Install, top right, opens a dialog
// where the admin names the manifest's git source, the server reads it (and
// stores nothing), the dialog shows everything it asks for, and Install sends
// the reviewed manifest's hash with an image pinned by digest. Who publishes
// a plugin changes nothing: it is untrusted code either way. A refusal from
// the server is shown verbatim.
import { createResource, createSignal, For, Show } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import {
  fetchPluginManifest,
  installPlugin,
  listConnections,
  listPlugins,
  type FetchedPluginManifest,
  type PluginInstallation,
} from "../api/client";
import {
  imageError,
  imageRepository,
  imageRepositoryError,
  manifestImageFact,
  installNameError,
  manifestReview,
  manifestSourceError,
  pluginChips,
  pluginManageHref,
  shortDigest,
  suggestedInstallName,
  toInstallBody,
  toManifestFetchBody,
  toStorageBody,
  DEFAULT_STORAGE_MOUNT_PATH,
  type BadgeTone,
  type ManifestFetchForm,
  type StorageForm,
} from "../plugins";
import { relTime, absTime } from "../fmt";
import Chips from "./Chips";
import Icon from "./Icon";
import Modal, { ModalMessage, ModalSection } from "./Modal";
import { Field, Input, Select } from "./Field";

/** A badge tone in the StatusBadge palette's classes. */
export const BADGE: Record<BadgeTone, string> = {
  ok: "badge badge-succeeded",
  motion: "badge badge-running",
  danger: "badge badge-failed",
  muted: "badge badge-skipped",
};

export default function Plugins(props: { org: string }) {
  const nav = useNavigate();
  const [plugins] = createResource(
    () => props.org,
    (o) => listPlugins(o),
  );
  const [installing, setInstalling] = createSignal(false);

  const data = () => (plugins.error ? undefined : plugins());
  const unavailable = () => !plugins.loading && !plugins.error && plugins() === null;
  const rows = () => data() ?? [];

  return (
    <div class="panel">
      <div class="panel-h">
        <span>
          Plugins · {props.org} <span class="tok-count">{rows().length}</span>
        </span>
        <Show when={!unavailable()}>
          <button class="btn btn-primary btn-sm" onClick={() => setInstalling(true)}>
            <Icon icon="plus" size={14} /> Install plugin
          </button>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={plugins.error}>
          <p class="error">Could not load the org's plugins.</p>
        </Show>

        <Show when={!plugins.loading || data()} fallback={<p class="empty">loading…</p>}>
          <Show when={!unavailable()} fallback={<p class="empty">This server does not run plugins.</p>}>
            <Show
              when={rows().length > 0}
              fallback={
                <p class="empty">
                  No plugins installed. A plugin connects Scarab to a vendor tool, such as Argo CD.
                  To install one, choose Install plugin and enter the git source of its manifest and
                  the image to run.
                </p>
              }
            >
              <table class="table lib-table" aria-label="Installed plugins">
                <thead>
                  <tr>
                    <th>Installation</th>
                    <th>Image</th>
                    <th>State</th>
                    <th>Approved</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  <For each={rows()}>
                    {(p) => (
                      <tr classList={{ dead: p.state === "killed" }}>
                        <td>
                          <A class="mono oenv-link" href={pluginManageHref(props.org, p)}>
                            {p.name}
                          </A>{" "}
                          <span class="subtle mono" title="The plugin's name, from its manifest">
                            · {p.plugin}
                          </span>
                        </td>
                        <td class="mono" title={p.image}>
                          {imageRepository(p.image).split("/").pop()} · {shortDigest(p.image)}
                        </td>
                        <td class="keep">
                          <Chips chips={pluginChips(p)} />
                        </td>
                        <td title={absTime(p.approved_at)}>
                          <span class="mono">{p.approved_by}</span>{" "}
                          <span class="subtle">· {relTime(p.approved_at)}</span>
                        </td>
                        <td class="acts keep">
                          <A class="btn btn-ghost btn-sm" href={pluginManageHref(props.org, p)}>
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
                <Icon icon="shield-check" size={12} /> An installation approves one image digest
                and exactly what its manifest asks for. An upgrade to a new digest is new code and is
                confirmed like an install; only narrowing the manifest on the installed digest is one
                click. A plugin acts
                as <span class="mono">plugin:&lt;name&gt;</span>, never as a person. Installing asks
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
          onInstalled={(p: PluginInstallation) => {
            setInstalling(false);
            nav(pluginManageHref(props.org, p));
          }}
        />
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
    const named = imageRepositoryError(image(), f?.manifest.image);
    if (named) return { error: named };
    return toInstallBody(
      { name: name(), image: image(), config: config(), storage: storage() },
      f ? { source: f.source, manifest_sha256: f.manifest_sha256, connection: fetchedVia() || undefined } : null,
      { taken: props.taken },
    );
  };
  // Quiet while a field is untouched.
  const nameProblem = () => (name() ? installNameError(name(), props.taken) : null);
  const imageProblem = () => imageError(image().trim()) ?? imageRepositoryError(image(), fetched()?.manifest.image);
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
                Pinned by digest: <span class="mono">&lt;repository&gt;@sha256:&lt;64 hex&gt;</span>, of
                the repository the manifest names when it names one. A tag can change under the same
                name; a digest cannot.
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
      <dl class="kv">
        <dt>Plugin</dt>
        <dd class="m">{r().plugin}</dd>
        <dt>Source</dt>
        <dd class="m">{props.fetched.source}</dd>
        <dt>Manifest SHA-256</dt>
        <dd class="m">{props.fetched.manifest_sha256}</dd>
        <dt>Image</dt>
        <dd
          class={manifestImageFact(props.fetched.manifest).warn ? "plg-warn" : "m"}
          data-testid="review-image"
        >
          <Show
            when={!manifestImageFact(props.fetched.manifest).warn}
            fallback={
              <>
                <Icon icon="alert-triangle" size={12} /> {manifestImageFact(props.fetched.manifest).text}
              </>
            }
          >
            {manifestImageFact(props.fetched.manifest).text}
          </Show>
        </dd>
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
