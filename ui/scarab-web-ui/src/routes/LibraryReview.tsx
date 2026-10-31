// Install or upgrade a library (ADR-0076 §4) — a page of its own, so a
// second reviewer can be sent its URL. The source is fetched (the server
// reads it at its commit, checks it compiles as a closed library and stores
// it); the page shows the file, or the side-by-side diff from the installed
// version, the checks, and what changes. Install then installs exactly the
// version shown, naming the version it replaces, and asks for a step-up.
import { createEffect, createResource, createSignal, For, Show } from "solid-js";
import { A, useNavigate, useSearchParams } from "@solidjs/router";
import {
  fetchLibrary,
  getMe,
  installLibrary,
  listConnections,
  listLibraries,
  type FetchedLibrary,
} from "../api/client";
import {
  diffLines,
  diffSides,
  foldDiff,
  inputsChange,
  numberDiff,
  reviewChecks,
  shortHash,
  sideBySide,
  sourceError,
  toFetchBody,
  type FetchForm,
  type SideCell,
} from "../libraries";
import Icon from "../components/Icon";

const one = (v: string | string[] | undefined) => (Array.isArray(v) ? v[0] ?? "" : v ?? "");

export default function LibraryReview() {
  const nav = useNavigate();
  const [search, setSearch] = useSearchParams();
  const [me] = createResource(getMe);
  const org = () => me()?.admin_orgs[0];
  const [libs] = createResource(org, (o) => listLibraries(o));
  const [connections] = createResource(() => listConnections().catch(() => []));
  const [form, setForm] = createSignal<FetchForm>({
    source: one(search.source),
    connection: one(search.connection),
    publisher: one(search.publisher),
    name: one(search.name),
  });
  const [editing, setEditing] = createSignal(one(search.source) === "");
  const [fetched, setFetched] = createSignal<FetchedLibrary | null>(null);
  const [fetchedAt, setFetchedAt] = createSignal<number | null>(null);
  const [busy, setBusy] = createSignal<"" | "fetch" | "install">("");
  const [error, setError] = createSignal<string | null>(null);
  const set = (k: keyof FetchForm, v: string) => {
    setFetched(null);
    setForm((f) => ({ ...f, [k]: v }));
  };
  const built = () => toFetchBody(form());

  const doFetch = async () => {
    const o = org();
    const body = built().body;
    if (!o || !body || busy()) return;
    setBusy("fetch");
    setError(null);
    try {
      const f = await fetchLibrary(o, body);
      setFetched(f);
      setFetchedAt(Date.now());
      setEditing(false);
      // The URL names what was fetched, so it can be sent to a reviewer.
      setSearch(
        { source: f.source, publisher: f.publisher, name: f.name, connection: form().connection || "" },
        { replace: true },
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not fetch the library.");
      setEditing(true);
    } finally {
      setBusy("");
    }
  };

  // Opened with a source (an upgrade's "review", a shared link): fetch it.
  let fetchedOnce = false;
  createEffect(() => {
    if (fetchedOnce || !org() || !built().body || one(search.source) === "") return;
    fetchedOnce = true;
    void doFetch();
  });

  const doInstall = async () => {
    const o = org();
    const f = fetched();
    if (!o || !f || busy()) return;
    setBusy("install");
    setError(null);
    try {
      await installLibrary(o, f.publisher, f.name, {
        content_hash: f.content_hash,
        replaces: f.current?.content_hash ?? null,
      });
      nav("/settings");
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not install the library.");
    } finally {
      setBusy("");
    }
  };

  const lines = () => diffLines(fetched()?.diff ?? "");
  const sides = () => sideBySide(foldDiff(numberDiff(lines()), 2));
  const counts = () => ({
    add: lines().filter((l) => l.sign === "+").length,
    del: lines().filter((l) => l.sign === "-").length,
  });
  const same = () => {
    const f = fetched();
    return !!f && f.current?.content_hash === f.content_hash;
  };
  const checks = () => {
    const f = fetched();
    return f ? reviewChecks(f.content, f.first_party, libs()?.installations ?? []) : [];
  };
  const title = () => {
    const f = fetched();
    const p = f?.publisher || form().publisher;
    const n = f?.name || form().name;
    return p && n ? `lib:${p}/${n}` : "Install a library";
  };
  const connectionLabel = () => {
    const c = form().connection;
    if (!c) return "none: read anonymously";
    const conn = (connections() ?? []).find((x) => x.id === c);
    return conn ? `${conn.id} (${conn.kind}, ${conn.base_url})` : c;
  };

  return (
    <section class="page">
      <div class="run-head">
        <h1 class="crumb-head">
          <A href="/settings" class="crumb-head-link">
            Settings
          </A>
          <Icon icon="chevron-right" size={20} class="crumb-head-sep" />
          <span class="crumb-head-title mono">{title()}</span>
        </h1>
      </div>
      <p class="page-sub">
        {fetched()?.current ? "review an upgrade" : "review a library before installing it"}
        <Show when={fetchedAt()}> · fetched just now by {me()?.subject}</Show>
      </p>

      <Show when={me()} fallback={<p class="empty">loading…</p>}>
        <Show when={org()} fallback={<p class="empty">No organization yet: bind a repository first.</p>}>
          <div class="lib-review">
            <div>
              <div class="panel" style={{ "margin-top": 0 }}>
                <div class="panel-h">
                  <span>Source</span>
                  <span class="grow1" />
                  <Show when={!editing()}>
                    <button class="btn btn-ghost btn-sm" onClick={() => setEditing(true)}>
                      edit
                    </button>
                  </Show>
                </div>
                <Show
                  when={editing()}
                  fallback={
                    <dl class="lib-facts">
                      <dt>source</dt>
                      <dd class="m">{fetched()?.source ?? form().source}</dd>
                      <dt>connection</dt>
                      <dd>{connectionLabel()}</dd>
                      <dt>publisher</dt>
                      <dd class="m">
                        {fetched()?.publisher || form().publisher || "the repository owner"}
                      </dd>
                    </dl>
                  }
                >
                  <div class="secrets-body token-form">
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
                        A file in a git repository at a full commit id. A public repository on
                        github.com or a Forgejo connection's host is read without credentials.
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
                        Pipelines name it{" "}
                        <span class="mono">
                          lib:{form().publisher || "<owner>"}/{form().name || "<file>"}
                        </span>
                        .
                      </p>
                    </div>
                    <Show when={built().error && form().source.trim() !== "" && !sourceError(form().source)}>
                      <p class="error">{built().error}</p>
                    </Show>
                    <div class="modal-actions">
                      <button
                        class="btn btn-primary btn-sm"
                        disabled={!built().body || busy() !== ""}
                        onClick={() => void doFetch()}
                      >
                        {busy() === "fetch" ? "Fetching…" : "Fetch and review"}
                      </button>
                    </div>
                  </div>
                </Show>
              </div>

              <Show when={error()}>
                <p class="error">{error()}</p>
              </Show>
              <Show when={busy() === "fetch"}>
                <p class="empty">Fetching {form().source}…</p>
              </Show>

              <Show when={fetched()}>
                {(f) => (
                  <Show
                    when={f().current}
                    fallback={
                      <>
                        <p class="pane-sub" style={{ "margin-top": "18px" }}>
                          content · {f().content.split("\n").length} lines
                        </p>
                        <div class="ud">
                          <For each={f().content.split("\n")}>
                            {(t, k) => (
                              <div class="ud-row" style={{ "grid-template-columns": "34px 1fr" }}>
                                <span class="ln">{k() + 1}</span>
                                <span class="tx">{t}</span>
                              </div>
                            )}
                          </For>
                        </div>
                      </>
                    }
                  >
                    {(cur) => (
                      <Show when={!same()} fallback={<p class="empty">This version is installed already.</p>}>
                        <p class="pane-sub" style={{ "margin-top": "18px" }}>
                          diff · {counts().add} lines added, {counts().del} removed · unchanged lines folded
                        </p>
                        <div class="sbs">
                          <div>
                            <div class="sbs-h">
                              installed <span class="lib-hash">{shortHash(cur().content_hash)}</span> · @
                              {shortHash(cur().source.split("@").pop() ?? "")}
                            </div>
                            <div class="ud">
                              <For each={sides().left}>{(c) => <Cell c={c} side="old" />}</For>
                            </div>
                          </div>
                          <div>
                            <div class="sbs-h">
                              fetched <span class="lib-hash">{shortHash(f().content_hash)}</span> · @
                              {shortHash(f().source.split("@").pop() ?? "")}
                            </div>
                            <div class="ud">
                              <For each={sides().right}>{(c) => <Cell c={c} side="new" />}</For>
                            </div>
                          </div>
                        </div>
                      </Show>
                    )}
                  </Show>
                )}
              </Show>
            </div>

            <div class="lib-side">
              <Show when={fetched()}>
                {(f) => (
                  <>
                    <div class="panel">
                      <div class="panel-h">
                        <span>Checks</span>
                      </div>
                      <div style={{ padding: "12px 14px" }}>
                        <ul class="lib-checks">
                          <For each={checks()}>
                            {(c) => (
                              <li class={c.tone}>
                                <Icon icon={c.tone === "ok" ? "check" : c.tone === "warn" ? "alert-triangle" : "search"} size={12} />
                                <span>{c.text}</span>
                              </li>
                            )}
                          </For>
                        </ul>
                      </div>
                    </div>
                    <div class="panel">
                      <div class="panel-h">
                        <span>What changes</span>
                      </div>
                      <dl class="lib-facts">
                        <dt>version</dt>
                        <dd class="m">
                          <Show when={f().current} fallback={<span class="lib-hash">{shortHash(f().content_hash)}</span>}>
                            {(cur) => (
                              <>
                                <span class="lib-hash">{shortHash(cur().content_hash)}</span> →{" "}
                                <span class="lib-hash">{shortHash(f().content_hash)}</span>
                              </>
                            )}
                          </Show>
                        </dd>
                        <dt>inputs</dt>
                        <dd>
                          {f().current
                            ? inputsChange(diffSides(lines()).old, f().content)
                            : inputsChange("", f().content)}
                        </dd>
                        <dt>runs</dt>
                        <dd>Runs already created keep the Steps they were compiled with.</dd>
                      </dl>
                      <div style={{ padding: "0 14px 14px", display: "flex", gap: "8px" }}>
                        <button
                          class="btn btn-primary btn-sm"
                          disabled={busy() !== "" || same()}
                          onClick={() => void doInstall()}
                        >
                          <Icon icon={f().current ? "arrow-up" : "plus"} size={14} />{" "}
                          {busy() === "install" ? "Installing…" : f().current ? "Upgrade" : "Install"}
                        </button>
                        <button class="btn btn-ghost btn-sm" onClick={() => nav("/settings")}>
                          Discard
                        </button>
                      </div>
                    </div>
                  </>
                )}
              </Show>
              <p class="subtle">
                <small>
                  <Icon icon="shield-check" size={12} /> Installing asks you to confirm with your
                  passkey. The fetched version is stored for review; nothing is installed until you
                  confirm.
                </small>
              </p>
            </div>
          </div>
        </Show>
      </Show>
    </section>
  );
}

/** One side-by-side diff cell. */
function Cell(props: { c: SideCell; side: "old" | "new" }) {
  const c = props.c;
  if ("fold" in c) return <div class="ud-row fold">⋯ {c.fold} unchanged {c.fold === 1 ? "line" : "lines"}</div>;
  if ("blank" in c) {
    return (
      <div class="ud-row blank">
        <span class="ln" />
        <span class="tx"> </span>
      </div>
    );
  }
  return (
    <div class={`ud-row ${c.sign === "+" ? "add" : c.sign === "-" ? "del" : ""}`}>
      <span class="ln">{props.side === "old" ? c.old : c.new}</span>
      <span class="tx">{c.text}</span>
    </div>
  );
}
