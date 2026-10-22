// Plugins (ADR-0072, Deployments MMP d1) — the Settings section where an org
// admin installs a plugin, sees whether it is running, kills it, and wires
// what it reports into Environments.
//
// A PluginInstallation is the approval of exactly (image digest, manifest), so
// the install form asks for a digest and shows what the manifest asks for. The
// MMP installs first-party plugins only; anything else is the server's 403,
// shown verbatim. Kill is the switch ADR-0072 §10 promises: it revokes at the
// token-to-installation mapping, so it bites on the next request rather than
// when a token expires. Uninstall retires the name forever — a new install
// under it would silently inherit the old one's Deployment record.
//
// "Unmapped groupings" are groupings the plugin reports on that no
// Environment lists: their reports are dropped. Each one offers to add itself
// as a source on an org Environment — preselected only when an Environment is
// named like the grouping; otherwise the admin picks (a guess would route
// reports somewhere nobody chose). The targets already reported under it
// reach the board when the plugin re-sends them (within about a minute).
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { Portal } from "solid-js/web";
import {
  enablePlugin,
  installPlugin,
  killPlugin,
  listOrgEnvironments,
  listPluginCatalog,
  listPlugins,
  listUnmappedGroupings,
  putOrgEnvironment,
  uninstallPlugin,
  type OrgEnvironment,
  type PluginCatalogEntry,
  type PluginInstallation,
} from "../api/client";
import {
  imageError,
  imageRepository,
  installNameError,
  runtimeBadge,
  shortDigest,
  sourceAddedNotice,
  stateBadge,
  suggestEnvironment,
  toInstallBody,
  usageLine,
  withSource,
  type BadgeTone,
} from "../plugins";
import { relTime, absTime } from "../fmt";
import Icon from "./Icon";

/** A badge tone in the StatusBadge palette's classes. */
const BADGE: Record<BadgeTone, string> = {
  ok: "badge badge-succeeded",
  motion: "badge badge-running",
  danger: "badge badge-failed",
  muted: "badge badge-skipped",
};

export default function Plugins(props: { org: string; onEnvironmentsChanged?: () => void }) {
  const [epoch, setEpoch] = createSignal(0);
  const [plugins] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listPlugins(k.org),
  );
  // For the unmapped-grouping shortcut; without it the shortcut just hides.
  const [orgEnvs, { refetch: refetchEnvs }] = createResource(
    () => props.org,
    (o) => listOrgEnvironments(o).catch(() => null),
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

  const kill = (p: PluginInstallation) => {
    if (
      !window.confirm(
        `Kill ${p.name}?\n\nThis revokes it immediately: from the next request on, its deployment reports and broker calls are refused, even with a token that has not expired. Nothing is uninstalled — Enable brings it back.`,
      )
    )
      return;
    void act(() => killPlugin(props.org, p.name), `Could not kill ${p.name}.`, `${p.name} is killed.`);
  };

  const enable = (p: PluginInstallation) =>
    void act(() => enablePlugin(props.org, p.name), `Could not enable ${p.name}.`, `${p.name} is enabled again.`);

  const uninstall = (p: PluginInstallation) => {
    const n = p.deployment_sources.length;
    if (
      !window.confirm(
        `Uninstall ${p.name}?\n\nThe name ${p.name} is retired forever — it can never be installed again, so nobody can inherit its deployment history by reusing it.${
          n > 0 ? `\n\n${n} environment source${n === 1 ? "" : "s"} naming it will be removed with it.` : ""
        }`,
      )
    )
      return;
    setError(null);
    setNotice(null);
    uninstallPlugin(props.org, p.name).then(
      (r) => {
        const removed = r.removed_deployment_sources.length;
        setNotice(
          `${r.name} is uninstalled; its name is retired. ${removed} environment source${removed === 1 ? "" : "s"} removed.`,
        );
        reload();
        void refetchEnvs();
        if (removed > 0) props.onEnvironmentsChanged?.();
      },
      (e) => setError(e instanceof Error ? e.message : `Could not uninstall ${p.name}.`),
    );
  };

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
                  No plugins installed. A plugin brings vendor knowledge Scarab does not own —
                  the Argo CD plugin, say, reports what is live on each target into the
                  Deployment record.
                </p>
              }
            >
              <ul class="tok-list">
                <For each={rows()}>
                  {(p) => (
                    <PluginRow
                      org={props.org}
                      plugin={p}
                      orgEnvs={orgEnvs() ?? null}
                      onKill={() => kill(p)}
                      onEnable={() => enable(p)}
                      onUninstall={() => uninstall(p)}
                      onSourceAdded={(msg) => {
                        setNotice(msg);
                        reload();
                        void refetchEnvs();
                        props.onEnvironmentsChanged?.();
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
                and exactly what its manifest asks for — a new digest is a new install. Only
                first-party plugins can be installed today. A plugin acts as{" "}
                <span class="mono">plugin:&lt;name&gt;</span>, never as a person.
              </small>
            </p>
          </Show>
        </Show>
      </div>

      <Show when={installing()}>
        {/* Portalled: each `.page > *` panel is its own stacking context, so a
            dialog left inside one renders under the panels after it. */}
        <Portal>
          <InstallDialog
            org={props.org}
            taken={rows().map((p) => p.name)}
            onClose={() => setInstalling(false)}
            onInstalled={(p) => {
              setInstalling(false);
              const n = p.deployment_sources.length;
              setNotice(
                n > 0
                  ? `${p.name} is installed and already feeds ${n} environment source${n === 1 ? "" : "s"} written for it.`
                  : `${p.name} is installed. Add it as a source on an environment for its reports to land.`,
              );
              reload();
            }}
          />
        </Portal>
      </Show>
    </div>
  );
}

/** One installation: identity, approval, runtime, and where it reports. */
function PluginRow(props: {
  org: string;
  plugin: PluginInstallation;
  orgEnvs: OrgEnvironment[] | null;
  onKill: () => void;
  onEnable: () => void;
  onUninstall: () => void;
  onSourceAdded: (msg: string) => void;
}) {
  const p = () => props.plugin;
  const killed = () => p().state === "killed";
  const state = () => stateBadge(p().state);
  const runtime = () => runtimeBadge(p().runtime_status);
  const usage = () => usageLine(p().usage);
  const [open, setOpen] = createSignal(false);

  return (
    <li class="tok" classList={{ "tok-dead": killed() }}>
      <div class="tok-head">
        <Icon icon="boxes" size={15} />
        <span class="tok-name mono">{p().name}</span>
        <span class="tok-scope mono" title="The catalog plugin">
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
        <span class={usage().warn ? "plg-killed" : undefined} title={usage().title}>
          <Icon icon="database" size={12} />
          {usage().label}
        </span>
      </div>

      <div class="tok-facts">
        <span>
          <Icon icon="waypoints" size={12} />
          <Show
            when={p().deployment_sources.length > 0}
            fallback={"no environment lists it as a source — its reports are dropped"}
          >
            <span class="oenv-sources">
              <For each={p().deployment_sources}>
                {(s) => (
                  <span class="mono" title="Environment ← vendor grouping">
                    {s.environment} ← {s.vendor_grouping}
                  </span>
                )}
              </For>
            </span>
          </Show>
        </span>
        <button class="btn btn-ghost btn-sm plg-toggle" onClick={() => setOpen((o) => !o)}>
          <Icon icon={open() ? "chevron-down" : "chevron-right"} size={12} /> unmapped groupings
        </button>
      </div>

      <Show when={open()}>
        <Unmapped
          org={props.org}
          installation={p().name}
          orgEnvs={props.orgEnvs}
          onSourceAdded={props.onSourceAdded}
        />
      </Show>
    </li>
  );
}

/** The groupings an installation reports on that no Environment lists, each
 *  with a shortcut to add it as a source. */
function Unmapped(props: {
  org: string;
  installation: string;
  orgEnvs: OrgEnvironment[] | null;
  onSourceAdded: (msg: string) => void;
}) {
  const [epoch, setEpoch] = createSignal(0);
  const [groupings] = createResource(
    () => ({ org: props.org, name: props.installation, epoch: epoch() }),
    (k) => listUnmappedGroupings(k.org, k.name),
  );
  const [error, setError] = createSignal<string | null>(null);
  const envs = () => props.orgEnvs ?? [];

  const add = async (grouping: string, envName: string) => {
    setError(null);
    try {
      // Re-read first: the PUT is a FULL replace, and the copy this panel
      // holds may predate an edit made in the Environments panel since.
      const env = (await listOrgEnvironments(props.org))?.find((e) => e.name === envName);
      if (!env) throw new Error(`${envName} is no longer registered.`);
      const built = withSource(env, props.installation, grouping);
      if (!built.body) throw new Error(built.error);
      await putOrgEnvironment(props.org, env.name, built.body);
      setEpoch((e) => e + 1);
      props.onSourceAdded(sourceAddedNotice(env.name, props.installation, grouping));
    } catch (e) {
      setError(e instanceof Error ? e.message : `Could not add the source to ${envName}.`);
    }
  };

  return (
    <div class="plg-unmapped">
      <Show when={error()}>
        <p class="error">{error()}</p>
      </Show>
      <Show when={!groupings.loading} fallback={<p class="empty">loading…</p>}>
        <Show when={!groupings.error} fallback={<p class="error">Could not load unmapped groupings.</p>}>
          <For
            each={groupings() ?? []}
            fallback={<p class="empty">Nothing dropped: every grouping it reports on is mapped.</p>}
          >
            {(g) => {
              // Preselected only on a name match; otherwise blank until the
              // admin picks — never the first Environment by default.
              const [pick, setPick] = createSignal(suggestEnvironment(g.vendor_grouping, envs()) ?? "");
              return (
                <div class="plg-unmapped-row">
                  <span class="mono plg-grouping">{g.vendor_grouping}</span>
                  <span class="subtle" title={`first ${absTime(g.first_at)} · last ${absTime(g.last_at)}`}>
                    {g.reports} report{g.reports === 1 ? "" : "s"} dropped · last {relTime(g.last_at)} ·{" "}
                    <span class="mono">{g.last_target_id}</span>
                  </span>
                  <Show
                    when={envs().length > 0}
                    fallback={<span class="subtle">register an environment above to route it</span>}
                  >
                    <span class="plg-add">
                      <select class="input" value={pick()} onChange={(e) => setPick(e.currentTarget.value)}>
                        <option value="" disabled>
                          route to…
                        </option>
                        <For each={envs()}>
                          {(e) => (
                            <option value={e.name} selected={e.name === pick()}>
                              {e.name}
                            </option>
                          )}
                        </For>
                      </select>
                      <button
                        class="btn btn-ghost btn-sm"
                        disabled={!pick()}
                        title={
                          pick()
                            ? `Add ${props.installation} / ${g.vendor_grouping} to ${pick()}'s deployment sources`
                            : "Pick the environment its reports belong in"
                        }
                        onClick={() => pick() && void add(g.vendor_grouping, pick())}
                      >
                        <Icon icon="plus" size={12} /> add as source
                      </button>
                    </span>
                  </Show>
                </div>
              );
            }}
          </For>
        </Show>
      </Show>
    </div>
  );
}

/** Install a catalog plugin, pinned by digest. */
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

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") props.onClose();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  const entries = () => (catalog() ?? []).filter((c) => c.first_party);
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
  const fieldProblem = () => {
    const img = image().trim();
    const untouched = !!entry() && img === `${entry()!.image_repository}@sha256:`;
    return (
      (name() ? installNameError(name(), props.taken) : null) ??
      (untouched ? null : imageError(img, entry()?.image_repository))
    );
  };

  const install = async () => {
    const body = built().body;
    if (!body || saving()) return;
    setSaving(true);
    setError(null);
    try {
      props.onInstalled(await installPlugin(props.org, body));
    } catch (e) {
      // The 403 ("third-party installs are off (ADR-0072): …") and the 409
      // (name taken or retired, or plugins off) are the whole answer.
      setError(e instanceof Error ? e.message : "Could not install the plugin.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal modal-tall" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h">
          <span>Install plugin</span>
        </div>
        <div class="modal-body token-form">
          <Show when={catalog.error}>
            <p class="error">Could not load the plugin catalog.</p>
          </Show>
          <Show when={!catalog.loading && !catalog.error && entries().length === 0}>
            <p class="empty">This server has no plugins to install.</p>
          </Show>

          <div class="form-r">
            <label>plugin</label>
            <select class="input" value={plugin()} onChange={(e) => choose(e.currentTarget.value)}>
              <option value="">choose a first-party plugin…</option>
              <For each={entries()}>{(c) => <option value={c.plugin}>{c.plugin}</option>}</For>
            </select>
            <Show when={entry()}>
              {(c) => <p class="fieldnote">{c().description}</p>}
            </Show>
          </div>

          <div class="form-r">
            <label>name</label>
            <input
              class="input mono"
              placeholder="argocd-prod"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
            />
            <p class="fieldnote">
              A DNS-1123 label: lowercase letters, digits and "-", at most 63. Its reports carry
              it as their source, and it acts as <span class="mono">plugin:{name() || "<name>"}</span>.
              Unique in the org and never reused — not even after an uninstall.
            </p>
          </div>

          <div class="form-r">
            <label>image</label>
            <input
              class="input mono"
              placeholder="ghcr.io/…@sha256:…"
              value={image()}
              onInput={(e) => setImage(e.currentTarget.value)}
            />
            <p class="fieldnote">
              Pinned by digest — paste the <span class="mono">sha256:</span> of the release you are
              approving. A tag can change under the same name; a digest cannot.
            </p>
          </div>

          <Show when={entry()}>
            {(c) => (
              <>
                <For each={c().manifest.config_keys}>
                  {(key) => (
                    <div class="form-r">
                      <label class="mono">{key}</label>
                      <input
                        class="input"
                        placeholder="optional"
                        value={config()[key] ?? ""}
                        onInput={(e) => setConfig((cfg) => ({ ...cfg, [key]: e.currentTarget.value }))}
                      />
                    </div>
                  )}
                </For>
                <div class="form-r">
                  <label>you are approving</label>
                  <div class="plg-caps">
                    <For each={c().manifest.capabilities}>
                      {(cap) => <span class="dep-tag dep-tag-muted">{cap.replace(/_/g, " ")}</span>}
                    </For>
                  </div>
                  <For each={c().manifest.connections}>
                    {(slot) => (
                      <p class="fieldnote">
                        connection <span class="mono">{slot.name}</span> ({slot.auth_schemes.join(", ")}) —
                        read only:{" "}
                        <span class="mono">
                          {slot.read_rules.map((r) => `${r.method} ${r.path}`).join(", ")}
                        </span>
                      </p>
                    )}
                  </For>
                </div>
              </>
            )}
          </Show>
        </div>

        <div class="modal-foot">
          <Show when={fieldProblem()}>
            <p class="error">{fieldProblem()}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={!built().body || saving()} onClick={install}>
              <Icon icon="plus" size={14} /> {saving() ? "Installing…" : "Install"}
            </button>
            <button class="btn btn-ghost" onClick={props.onClose}>
              Cancel
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
