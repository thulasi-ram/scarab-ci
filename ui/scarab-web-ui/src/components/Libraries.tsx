// Libraries (ADR-0076 §4-§6) — the Settings section where an org admin
// installs a library from a git source, upgrades it after reading the diff,
// removes it, and yanks a version.
//
// Installing is two steps. "Fetch" reads the source at its commit, checks it
// is a library that invokes only installed libraries, and shows its content
// (or the diff from the installed version); "Install" then installs exactly
// the version shown, and asks for a step-up. Removing and yanking tighten and
// ask for nothing; lifting a yank asks for a step-up.
//
// First-party libraries come from Scarab's release manifest. A new org gets
// them when its first repo is bound; if the source could not be read, the
// panel lists what is missing, and the admin can retry, point at a mirror, or
// paste a copy (checked against the manifest's hash).
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { Portal } from "solid-js/web";
import {
  fetchLibrary,
  installLibrary,
  listConnections,
  listLibraries,
  removeLibrary,
  retryLibraryDefaults,
  unyankLibrary,
  yankLibrary,
  type FetchedLibrary,
  type LibraryInstallation,
} from "../api/client";
import {
  defaultsLine,
  diffLines,
  missingLabel,
  shortHash,
  sourceError,
  toFetchBody,
  updateLine,
  yankReasonError,
  type FetchForm,
} from "../libraries";
import { relTime, absTime } from "../fmt";
import Icon from "./Icon";

/** What the install dialog opens with. */
type Prefill = Partial<FetchForm>;

export default function Libraries(props: { org: string }) {
  const [epoch, setEpoch] = createSignal(0);
  const [libs] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listLibraries(k.org),
  );
  const [installing, setInstalling] = createSignal<Prefill | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);
  const reload = () => setEpoch((e) => e + 1);

  const unavailable = () => !libs.loading && !libs.error && libs() === null;
  const rows = () => libs()?.installations ?? [];
  const offered = () => (libs()?.first_party ?? []).filter((f) => !f.installed);

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

  const remove = (l: LibraryInstallation) => {
    const id = `lib:${l.publisher}/${l.name}`;
    if (
      !window.confirm(
        `Remove ${id}?\n\nPipelines that invoke it stop compiling until it is installed again. Runs already created keep the Steps they were compiled with.`,
      )
    )
      return;
    void act(() => removeLibrary(props.org, l.publisher, l.name), `Could not remove ${id}.`, `${id} is removed.`);
  };

  const yank = (l: LibraryInstallation) => {
    const reason = window.prompt(
      `Yank version ${shortHash(l.content_hash)} of lib:${l.publisher}/${l.name}?\n\nNew Runs that use it are refused, and so are Steps and gates of existing Runs that use it, Reruns and Retries included. The reason is shown with every refusal.`,
    );
    if (reason === null) return;
    const problem = yankReasonError(reason);
    if (problem) {
      setError(problem);
      return;
    }
    void act(
      () => yankLibrary(props.org, l.content_hash, reason.trim()),
      "Could not yank the version.",
      `Version ${shortHash(l.content_hash)} is yanked in ${props.org}.`,
    );
  };

  const unyank = (hash: string) =>
    void act(
      () => unyankLibrary(props.org, hash),
      "Could not lift the yank.",
      `Version ${shortHash(hash)} can run again.`,
    );

  return (
    <div class="panel">
      <div class="panel-h">
        <span>Libraries · {props.org}</span>
        <Show when={rows().length > 0}>
          <span class="tok-count">{rows().length}</span>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={libs.error}>
          <p class="error">Could not load the org's libraries.</p>
        </Show>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
        <Show when={notice()}>
          <p class="notice-ok">{notice()}</p>
        </Show>

        <Show when={!libs.loading} fallback={<p class="empty">loading…</p>}>
          <Show when={!unavailable()} fallback={<p class="empty">This server does not manage libraries.</p>}>
            <Defaults
              org={props.org}
              defaults={libs()?.defaults ?? null}
              onDone={(msg) => {
                setError(null);
                setNotice(msg);
                reload();
              }}
            />
            <Show when={libs()?.manifest_error}>
              {(e) => (
                <p class="subtle">
                  <small>
                    <Icon icon="alert-triangle" size={12} /> The release manifest could not be read,
                    so first-party libraries are not marked: {e()}
                  </small>
                </p>
              )}
            </Show>

            <Show
              when={rows().length > 0}
              fallback={
                <p class="empty">
                  No libraries installed. A Pipeline uses an installed library with{" "}
                  <span class="mono">invoke: lib:&lt;publisher&gt;/&lt;name&gt;</span>.
                </p>
              }
            >
              <ul class="tok-list">
                <For each={rows()}>
                  {(l) => (
                    <LibraryRow
                      library={l}
                      onUpgrade={(source) =>
                        setInstalling({ source: source ?? "", publisher: l.publisher, name: l.name })
                      }
                      onRemove={() => remove(l)}
                      onYank={() => yank(l)}
                      onUnyank={() => unyank(l.content_hash)}
                    />
                  )}
                </For>
              </ul>
            </Show>

            <Show when={offered().length > 0}>
              <p class="subtle">First-party libraries not installed here:</p>
              <ul class="tok-list">
                <For each={offered()}>
                  {(f) => (
                    <li class="tok">
                      <div class="tok-head">
                        <Icon icon="package" size={15} />
                        <span class="tok-name mono">
                          lib:{f.publisher}/{f.name}
                        </span>
                        <span class="tok-scope">{f.release}</span>
                        <span class="oenv-actions">
                          <button
                            class="btn btn-ghost btn-sm"
                            onClick={() =>
                              setInstalling({ source: f.source, publisher: f.publisher, name: f.name })
                            }
                          >
                            install
                          </button>
                        </span>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
            </Show>

            <Show when={(libs()?.yanks ?? []).some((y) => !rows().some((r) => r.content_hash === y.content_hash))}>
              <p class="subtle">Yanked versions not installed now:</p>
              <ul class="tok-list">
                <For each={(libs()?.yanks ?? []).filter((y) => !rows().some((r) => r.content_hash === y.content_hash))}>
                  {(y) => (
                    <li class="tok tok-dead">
                      <div class="tok-head">
                        <span class="mono">{shortHash(y.content_hash)}</span>
                        <span class="plg-msg">{y.reason}</span>
                        <span class="subtle" title={absTime(y.yanked_at)}>
                          by {y.yanked_by} · {relTime(y.yanked_at)}
                        </span>
                        <span class="oenv-actions">
                          <button class="btn btn-ghost btn-sm" onClick={() => unyank(y.content_hash)}>
                            lift yank
                          </button>
                        </span>
                      </div>
                    </li>
                  )}
                </For>
              </ul>
            </Show>

            <div class="secrets-actions">
              <button class="btn btn-primary btn-sm" onClick={() => setInstalling({})}>
                <Icon icon="plus" size={14} /> Install library
              </button>
            </div>
            <p class="subtle">
              <small>
                <Icon icon="shield-check" size={12} /> An installation is one file at one commit,
                stored in Scarab: a Pipeline never fetches it. Installing and upgrading, and lifting
                a yank, ask you to confirm with your passkey.
              </small>
            </p>
          </Show>
        </Show>
      </div>

      <Show when={installing()}>
        {(prefill) => (
          <Portal>
            <InstallDialog
              org={props.org}
              prefill={prefill()}
              onClose={() => setInstalling(null)}
              onInstalled={(l) => {
                setInstalling(null);
                setError(null);
                setNotice(`lib:${l.publisher}/${l.name} is installed at ${shortHash(l.content_hash)}.`);
                reload();
              }}
            />
          </Portal>
        )}
      </Show>
    </div>
  );
}

/** One installation: name, version, source, yank and update notice. */
function LibraryRow(props: {
  library: LibraryInstallation;
  onUpgrade: (source?: string) => void;
  onRemove: () => void;
  onYank: () => void;
  onUnyank: () => void;
}) {
  const l = () => props.library;
  const update = () => updateLine(l().update);
  return (
    <li class="tok" classList={{ "tok-dead": !!l().yanked }}>
      <div class="tok-head">
        <Icon icon="package" size={15} />
        <span class="tok-name mono">
          lib:{l().publisher}/{l().name}
        </span>
        <span class="tok-scope mono" title={l().content_hash}>
          {shortHash(l().content_hash)}
        </span>
        <Show when={l().first_party}>
          <span class="tok-role" title="This version is listed in Scarab's release manifest.">
            first-party
          </span>
        </Show>
        <Show when={l().yanked}>
          <span class="tok-state tok-revoked">yanked</span>
        </Show>
        <span class="oenv-actions">
          <button class="btn btn-ghost btn-sm" onClick={() => props.onUpgrade()}>
            upgrade
          </button>
          <Show
            when={l().yanked}
            fallback={
              <button class="btn btn-danger btn-sm" onClick={props.onYank}>
                yank
              </button>
            }
          >
            <button class="btn btn-ghost btn-sm" onClick={props.onUnyank}>
              lift yank
            </button>
          </Show>
          <button class="btn btn-ghost btn-sm" onClick={props.onRemove}>
            remove
          </button>
        </span>
      </div>
      <div class="tok-facts">
        <span>
          <Icon icon="git-commit-horizontal" size={12} />
          <span class="mono">{l().source}</span>
        </span>
        <span title={absTime(l().installed_at)}>
          <Icon icon="user" size={12} />
          installed by {l().installed_by} · {relTime(l().installed_at)}
        </span>
      </div>
      <Show when={l().yanked}>
        {(y) => (
          <div class="tok-facts">
            <span class="plg-killed" title={absTime(y().yanked_at)}>
              <Icon icon="alert-triangle" size={12} />
              yanked by {y().yanked_by}: {y().reason}
            </span>
          </div>
        )}
      </Show>
      <Show when={update()}>
        {(line) => (
          <div class="tok-facts">
            <span classList={{ "plg-killed": !!l().update?.security_fix }}>
              <Icon icon={l().update?.security_fix ? "alert-triangle" : "arrow-up"} size={12} />
              {line()}
            </span>
            <button class="btn btn-ghost btn-sm" onClick={() => props.onUpgrade(l().update?.source)}>
              review the upgrade
            </button>
          </div>
        )}
      </Show>
    </li>
  );
}

/** Missing first-party defaults, and the form that retries them. */
function Defaults(props: {
  org: string;
  defaults: { state: string; missing: { publisher: string; name: string; error: string }[]; mirror?: string | null } | null;
  onDone: (msg: string) => void;
}) {
  const [mirror, setMirror] = createSignal(props.defaults?.mirror ?? "");
  const [copies, setCopies] = createSignal<Record<string, string>>({});
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const line = () => defaultsLine(props.defaults);

  const retry = async () => {
    if (busy()) return;
    setBusy(true);
    setError(null);
    try {
      const files = Object.fromEntries(Object.entries(copies()).filter(([, v]) => v.trim() !== ""));
      const d = await retryLibraryDefaults(props.org, {
        mirror: mirror().trim() || null,
        files,
      });
      props.onDone(
        d.state === "installed"
          ? "The first-party libraries are installed."
          : "Some first-party libraries are still missing; see why below.",
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not install the first-party libraries.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Show when={line()}>
      {(text) => (
        <div class="plg-conns">
          <p class="plg-killed">
            <Icon icon="alert-triangle" size={12} /> {text()}
          </p>
          <For each={props.defaults?.missing ?? []}>
            {(m) => (
              <div class="form-r">
                <label class="mono">{missingLabel(m)}</label>
                <p class="fieldnote">{m.error}</p>
                <Show when={m.name}>
                  <textarea
                    class="input mono"
                    rows={3}
                    placeholder="or paste a copy of the file from the release"
                    value={copies()[`${m.publisher}/${m.name}`] ?? ""}
                    onInput={(e) =>
                      setCopies((c) => ({ ...c, [`${m.publisher}/${m.name}`]: e.currentTarget.value }))
                    }
                  />
                </Show>
              </div>
            )}
          </For>
          <Show when={props.defaults?.state === "missing"}>
            <div class="form-r">
              <label>mirror (optional)</label>
              <input
                class="input mono"
                placeholder="git.example.com/mirrors/scarab-ci"
                value={mirror()}
                onInput={(e) => setMirror(e.currentTarget.value)}
              />
              <p class="fieldnote">
                A copy of Scarab's repository on a forge Scarab can read. Each file is read at the
                same path and commit, and must have the hash the release lists.
              </p>
            </div>
            <Show when={error()}>
              <p class="error">{error()}</p>
            </Show>
            <div class="modal-actions">
              <button class="btn btn-primary btn-sm" disabled={busy()} onClick={() => void retry()}>
                <Icon icon="rotate-cw" size={14} /> {busy() ? "Installing…" : "Retry"}
              </button>
            </div>
          </Show>
        </div>
      )}
    </Show>
  );
}

/** Fetch a version, read it (or its diff), then install it. */
function InstallDialog(props: {
  org: string;
  prefill: Prefill;
  onClose: () => void;
  onInstalled: (l: LibraryInstallation) => void;
}) {
  const [connections] = createResource(() => listConnections().catch(() => []));
  const [form, setForm] = createSignal<FetchForm>({
    source: props.prefill.source ?? "",
    connection: props.prefill.connection ?? "",
    publisher: props.prefill.publisher ?? "",
    name: props.prefill.name ?? "",
  });
  const [fetched, setFetched] = createSignal<FetchedLibrary | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const set = (k: keyof FetchForm, v: string) => {
    setFetched(null);
    setForm((f) => ({ ...f, [k]: v }));
  };
  const built = () => toFetchBody(form());

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") props.onClose();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  const doFetch = async () => {
    const body = built().body;
    if (!body || busy()) return;
    setBusy(true);
    setError(null);
    try {
      setFetched(await fetchLibrary(props.org, body));
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not fetch the library.");
    } finally {
      setBusy(false);
    }
  };

  const doInstall = async () => {
    const f = fetched();
    if (!f || busy()) return;
    setBusy(true);
    setError(null);
    try {
      props.onInstalled(
        await installLibrary(props.org, f.publisher, f.name, {
          content_hash: f.content_hash,
          replaces: f.current?.content_hash ?? null,
        }),
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not install the library.");
    } finally {
      setBusy(false);
    }
  };

  const same = () => {
    const f = fetched();
    return !!f && f.current?.content_hash === f.content_hash;
  };

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal modal-tall" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h">
          <span>{props.prefill.name ? `Install lib:${props.prefill.publisher}/${props.prefill.name}` : "Install library"}</span>
        </div>
        <div class="modal-body token-form">
          <div class="form-r">
            <label>source</label>
            <input
              class="input mono"
              placeholder="github.com/acme/scarab-libs//deploy-helm.yaml@<commit>"
              value={form().source}
              onInput={(e) => set("source", e.currentTarget.value)}
            />
            <Show when={sourceError(form().source)}>
              <p class="error">{sourceError(form().source)}</p>
            </Show>
            <p class="fieldnote">
              A file in a git repository at a full commit id. A public repository on github.com or a
              Forgejo connection's host is read without credentials.
            </p>
          </div>
          <div class="form-r">
            <label>connection (private repository)</label>
            <select class="input" value={form().connection} onChange={(e) => set("connection", e.currentTarget.value)}>
              <option value="">none: read it anonymously</option>
              <For each={connections() ?? []}>
                {(c) => (
                  <option value={c.id}>
                    {c.id} ({c.kind}, {c.base_url})
                  </option>
                )}
              </For>
            </select>
            <p class="fieldnote">
              Through a connection, only a repository owned by {props.org}, or bound to it, can be
              read.
            </p>
          </div>
          <div class="form-r">
            <label>publisher and name (optional)</label>
            <div class="plg-caps">
              <input
                class="input mono"
                placeholder="the repository owner"
                value={form().publisher}
                onInput={(e) => set("publisher", e.currentTarget.value)}
              />
              <input
                class="input mono"
                placeholder="the file name"
                value={form().name}
                onInput={(e) => set("name", e.currentTarget.value)}
              />
            </div>
            <p class="fieldnote">
              Pipelines name it <span class="mono">lib:{form().publisher || "<owner>"}/{form().name || "<file>"}</span>.
            </p>
          </div>

          <Show when={fetched()}>
            {(f) => (
              <div class="form-r">
                <label>
                  {f().current ? `change from ${shortHash(f().current!.content_hash)}` : "content"} ·{" "}
                  <span class="mono">{shortHash(f().content_hash)}</span>
                  <Show when={f().first_party}> · first-party</Show>
                </label>
                <Show
                  when={f().current}
                  fallback={<pre class="mono lib-diff">{f().content}</pre>}
                >
                  <Show when={!same()} fallback={<p class="fieldnote">This version is installed already.</p>}>
                    <pre class="mono lib-diff">
                      <For each={diffLines(f().diff)}>
                        {(d) => (
                          <span classList={{ "lib-add": d.sign === "+", "lib-del": d.sign === "-" }}>
                            {d.sign}
                            {d.text}
                            {"\n"}
                          </span>
                        )}
                      </For>
                    </pre>
                  </Show>
                </Show>
              </div>
            )}
          </Show>
        </div>

        <div class="modal-foot">
          <Show when={built().error && form().source.trim() !== "" && !sourceError(form().source)}>
            <p class="error">{built().error}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <Show
              when={fetched() && !same()}
              fallback={
                <button class="btn btn-primary" disabled={!built().body || busy()} onClick={() => void doFetch()}>
                  {busy() ? "Fetching…" : "Fetch and review"}
                </button>
              }
            >
              <button class="btn btn-primary" disabled={busy()} onClick={() => void doInstall()}>
                <Icon icon="plus" size={14} /> {busy() ? "Installing…" : fetched()?.current ? "Upgrade" : "Install"}
              </button>
            </Show>
            <button class="btn btn-ghost" onClick={props.onClose}>
              Cancel
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
