// Install or upgrade a library: a page of its own inside the settings frame
// (Libraries marked in the sidebar), so a second reviewer can be sent its
// URL. The URL's query names the source (the Install library dialog builds
// it); the page fetches it (the server reads it at its commit, or at the
// commit a branch or tag points to now, checks it
// compiles as a closed library and stores it) and shows what it is: the
// facts and checks on one label and value grid, then the file, or the
// side-by-side diff from the installed version. Install, in the footer row,
// installs exactly the version shown, naming the version it replaces, and
// asks for a step-up.
//
// Every library takes this one path, whoever publishes it.
import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import { useNavigate, useParams, useSearchParams } from "@solidjs/router";
import { fetchLibrary, installLibrary, listConnections, listLibraries } from "../api/client";
import {
  diffLines,
  diffSides,
  foldDiff,
  inputsChange,
  libraryManageHref,
  numberDiff,
  reviewChecks,
  isFullCommit,
  shortHash,
  sideBySide,
  splitSource,
  toFetchBody,
  type FetchForm,
  type SideCell,
} from "../libraries";
import Icon from "../components/Icon";
import InstallLibraryDialog from "../components/LibraryInstall";
import { decodeParam, orgHref } from "../settings-nav";
import { me } from "../session";
import { SettingsCrumbs, SettingsFrame } from "./Settings";

const one = (v: string | string[] | undefined) => (Array.isArray(v) ? v[0] ?? "" : v ?? "");

/** `/orgs/:org/settings/libraries/review`, in the settings frame, which shows
 *  it only to an admin of the org. */
export default function LibraryReview() {
  const params = useParams<{ org: string }>();
  const org = () => decodeParam(params.org);
  return (
    <SettingsFrame kind="org" org={org()} section="libraries">
      {/* Keyed: the review holds what it fetched for one org, so another org
          in the URL starts a new review rather than installing into it. */}
      <Show when={org()} keyed>
        {(o) => <Review org={o} />}
      </Show>
    </SettingsFrame>
  );
}

function Review(props: { org: string }) {
  const nav = useNavigate();
  const [search] = useSearchParams();
  const back = () => orgHref(props.org, "libraries");
  const [libs] = createResource(() => props.org, (o) => listLibraries(o));
  const [connections] = createResource(
    () => props.org,
    (o) => listConnections(o).catch(() => []),
  );
  // What the URL asks to review. The fetch follows it: a changed source
  // (Change source, a shared link) fetches again.
  const form = createMemo<FetchForm>(() => ({
    source: one(search.source),
    ref: one(search.ref),
    connection: one(search.connection),
    name: one(search.name),
  }));
  const request = createMemo(() => {
    const body = toFetchBody(form()).body;
    return body ? JSON.stringify(body) : false;
  });
  const [fetching] = createResource(request, (b) => fetchLibrary(props.org, JSON.parse(b)));
  // Only a finished fetch of the source in the URL: while another source is
  // being fetched, the last one's version must not be installable.
  const fetched = () => (fetching.state === "ready" ? fetching() : undefined);
  const [changing, setChanging] = createSignal(false);
  const [installing, setInstalling] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const doInstall = async () => {
    const f = fetched();
    if (!f || installing()) return;
    setInstalling(true);
    setError(null);
    try {
      const done = await installLibrary(props.org, {
        name: f.name,
        content_hash: f.content_hash,
        replaces: f.current?.content_hash ?? null,
      });
      nav(libraryManageHref(props.org, done));
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not install the library.");
    } finally {
      setInstalling(false);
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
    return f ? reviewChecks(f.content, libs()?.installations ?? []) : [];
  };
  const title = () => {
    const f = fetched();
    const n = f?.name || form().name;
    return n ? `lib:${n}` : "Review a library";
  };
  /** The branch, tag or commit the URL asks for. */
  const askedRef = () => form().ref.trim() || splitSource(form().source).ref;
  const connectionLabel = () => {
    const c = form().connection;
    if (!c) return "none: read anonymously";
    const conn = (connections() ?? []).find((x) => x.id === c);
    return conn ? `${conn.id} (${conn.kind}, ${conn.base_url})` : c;
  };

  return (
    <>
      <SettingsCrumbs
        trail={[
          { label: "Libraries", href: back() },
          { label: title(), mono: title().startsWith("lib:") },
        ]}
        sub={
          <>
            <span class="mono">{props.org}</span> ·{" "}
            {fetched()?.current ? "review an upgrade" : "review a library before installing it"}
            <Show when={fetched()}> · fetched by {me()?.handle}</Show>
          </>
        }
      />

      <Show
        when={request()}
        fallback={
          <div class="panel">
            <div class="panel-h">
              <span>Nothing to review</span>
            </div>
            <p class="kv-note kv-lead">Give a library's source to fetch it and read it here before installing it.</p>
            <div class="panel-foot">
              <button class="btn btn-primary btn-sm" onClick={() => setChanging(true)}>
                <Icon icon="plus" size={14} /> Install library
              </button>
            </div>
          </div>
        }
      >
        <div class="rv">
          <div class="panel">
            <div class="panel-h">
              <span>{fetched()?.current ? "Upgrade" : "Install"}</span>
              <button class="btn btn-ghost btn-sm" onClick={() => setChanging(true)}>
                Change source
              </button>
            </div>
            <dl class="kv">
              <dt>Library</dt>
              <dd class="m">{title().startsWith("lib:") ? title() : "<owner>::<repo>::<file name>"}</dd>
              <dt>Source</dt>
              <dd class="m">{fetched()?.source ?? form().source}</dd>
              <Show when={askedRef() && !isFullCommit(askedRef())}>
                <dt>Ref</dt>
                <dd>
                  <span class="mono">{askedRef()}</span>
                  <Show when={fetched()}>
                    {(f) => (
                      <>
                        {" "}
                        at <span class="lib-hash">{shortHash(splitSource(f().source).ref)}</span> when fetched
                      </>
                    )}
                  </Show>
                </dd>
              </Show>
              <dt>Connection</dt>
              <dd>{connectionLabel()}</dd>
              <Show when={fetched()}>
                {(f) => (
                  <>
                    <dt>Version</dt>
                    <dd>
                      <Show
                        when={f().current}
                        fallback={<span class="lib-hash">{shortHash(f().content_hash)}</span>}
                      >
                        {(cur) => (
                          <>
                            <span class="lib-hash">{shortHash(cur().content_hash)}</span> →{" "}
                            <span class="lib-hash">{shortHash(f().content_hash)}</span>
                          </>
                        )}
                      </Show>
                    </dd>
                    <dt>Inputs</dt>
                    <dd>
                      {f().current ? inputsChange(diffSides(lines()).old, f().content) : inputsChange("", f().content)}
                    </dd>
                    <dt>Runs</dt>
                    <dd>Runs already created keep the Steps they were compiled with.</dd>
                    <dt>Checks</dt>
                    <dd>
                      <ul class="lib-checks">
                        <For each={checks()}>
                          {(c) => (
                            <li class={c.tone}>
                              <Icon
                                icon={c.tone === "ok" ? "check" : c.tone === "warn" ? "alert-triangle" : "search"}
                                size={12}
                              />
                              <span>{c.text}</span>
                            </li>
                          )}
                        </For>
                      </ul>
                    </dd>
                  </>
                )}
              </Show>
            </dl>
            <Show when={fetching.loading}>
              <p class="kv-note">Fetching…</p>
            </Show>
            <Show when={fetching.error}>
              <p class="kv-note error">
                {fetching.error instanceof Error ? fetching.error.message : "Could not fetch the library."}
              </p>
            </Show>
          </div>

          <Show when={fetched()}>
            {(f) => (
              <Show
                when={f().current}
                fallback={
                  <div class="panel">
                    <div class="panel-h">
                      <span>Content · {f().content.split("\n").length} lines</span>
                    </div>
                    <div class="ud rv-code">
                      <For each={f().content.split("\n")}>
                        {(t, k) => (
                          <div class="ud-row" style={{ "grid-template-columns": "34px 1fr" }}>
                            <span class="ln">{k() + 1}</span>
                            <span class="tx">{t}</span>
                          </div>
                        )}
                      </For>
                    </div>
                  </div>
                }
              >
                {(cur) => (
                  <Show when={!same()} fallback={<p class="empty">This version is installed already.</p>}>
                    <div class="panel">
                      <div class="panel-h">
                        <span>
                          Diff · {counts().add} lines added, {counts().del} removed · unchanged lines folded
                        </span>
                      </div>
                      <div class="sbs rv-code">
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
                    </div>
                  </Show>
                )}
              </Show>
            )}
          </Show>

          <div class="rv-foot">
            <button
              class="btn btn-primary btn-sm"
              disabled={!fetched() || installing() || same()}
              onClick={() => void doInstall()}
            >
              <Icon icon={fetched()?.current ? "arrow-up" : "plus"} size={14} />{" "}
              {installing() ? "Installing…" : fetched()?.current ? "Upgrade" : "Install"}
            </button>
            <button class="btn btn-ghost btn-sm" onClick={() => nav(back())}>
              Discard
            </button>
            <span class="rv-foot-note">
              <Show
                when={error()}
                fallback={
                  <>
                    <Icon icon="shield-check" size={12} /> Installing asks you to confirm with your passkey.
                    The fetched version is stored for review; nothing is installed until you confirm.
                  </>
                }
              >
                <span class="error">{error()}</span>
              </Show>
            </span>
          </div>
        </div>
      </Show>

      <Show when={changing()}>
        <InstallLibraryDialog org={props.org} initial={form()} onClose={() => setChanging(false)} />
      </Show>
    </>
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
