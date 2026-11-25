// Run-Pipeline flow — repo → ref → catalog → pick a pipeline → its run form
// in a dialog (RunPipelineDialog). One ref threads through; the catalog
// resolves it to a concrete SHA that the dialog reads the inputs at and the
// dispatch pins to, so the form and the run see the same config.
import { createResource, createSignal, createEffect, For, Show } from "solid-js";
import { useParams } from "@solidjs/router";
import { listRefs, listPipelines } from "../api/client";
import Icon from "../components/Icon";
import { HomeCrumb, RepoCrumb } from "../components/PageCrumbs";
import { Input } from "../components/Field";
import Doodle from "../components/Doodle";
import Pill from "../components/Pill";
import RunPipelineDialog from "../components/RunPipelineDialog";

export default function RunPipeline() {
  const params = useParams();
  const org = () => params.org!;
  const repo = () => params.repo!;

  // The picker starts on the repo's default branch, which the refs call
  // names; until it answers no catalog loads. The chosen ref threads through
  // every call.
  const [ref, setRef] = createSignal("");
  const [loadedRef, setLoadedRef] = createSignal<string>();
  const [selected, setSelected] = createSignal<string | null>(null);

  const [catalog] = createResource(loadedRef, (r) => listPipelines(org(), repo(), r));
  // Reading an errored resource throws, and with no error boundary the throw
  // leaves the page's computations stale: a later ref that loads fine would
  // never render. Every read of the catalog goes through this guard.
  const catalogValue = () => (catalog.error ? undefined : catalog());

  // The human catalog: manually-dispatchable pipelines, plus any file that
  // failed to parse (shown disabled/annotated, never crashing the list).
  const rows = () => (catalogValue()?.pipelines ?? []).filter((p) => p.manual || !!p.error);

  const loadCatalog = () => {
    setSelected(null);
    setLoadedRef(ref().trim() || "HEAD");
  };

  // ---- ref combobox -------------------------------------------------------
  // The repo's branches + tags, fetched once. Filtering is client-side over the
  // typed text (the list is small and it keeps the dropdown instant); a repo
  // with no forge / an error yields `[]`, so the field degrades to the plain
  // free-text entry it was before — you can still type a branch or full SHA.
  const [refs] = createResource(
    () => ({ org: org(), repo: repo() }),
    (k) =>
      listRefs(k.org, k.repo).catch(() => ({ refs: [], defaultBranch: null })),
  );

  // Start on the default branch once the refs call answers. A forge that
  // cannot name it leaves the field empty and reads the catalog at `HEAD`,
  // which the forge resolves to the default branch.
  createEffect(() => {
    const r = refs();
    if (!r || loadedRef() !== undefined) return;
    setRef(r.defaultBranch ?? "");
    setLoadedRef(r.defaultBranch ?? "HEAD");
  });
  const [refOpen, setRefOpen] = createSignal(false);
  const [refHi, setRefHi] = createSignal(0);

  const refMatches = () => {
    const q = ref().trim().toLowerCase();
    const all = refs()?.refs ?? [];
    return q ? all.filter((r) => r.name.toLowerCase().includes(q)) : all;
  };

  // Commit a picked ref and load its catalog in one gesture.
  const pickRef = (name: string) => {
    setRef(name);
    setRefOpen(false);
    loadCatalog();
  };

  const onRefKey = (e: KeyboardEvent) => {
    const ms = refMatches();
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setRefOpen(true);
      setRefHi((h) => Math.min(h + 1, ms.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setRefHi((h) => Math.max(h - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      // A highlighted suggestion wins; otherwise commit whatever was typed
      // (a branch name or a pasted full SHA) directly.
      if (refOpen() && ms.length > 0) pickRef(ms[Math.min(refHi(), ms.length - 1)].name);
      else loadCatalog();
    } else if (e.key === "Escape") {
      setRefOpen(false);
    }
  };

  return (
    <section class="page">
      <Doodle icon="rocket" size={230} rotate={12} opacity={0.16} top="52px" right="48px" />

      <div class="run-head">
        <h1 class="crumb-head">
          <HomeCrumb size={20} sepClass="crumb-head-sep" />
          <RepoCrumb org={org()} repo={repo()} link />
          <Icon icon="chevron-right" size={20} class="crumb-head-sep" />
          <span class="crumb-head-title">Run pipeline</span>
        </h1>
      </div>

      {/* ---- ref picker ---- */}
      {/* overflow-visible so the combobox dropdown can spill past the card edge
          (the base `.panel` clips with overflow:hidden for its rounded corners). */}
      <div class="panel panel-combo">
        <div class="panel-h"><span>Ref</span></div>
        <div class="rp-ref">
          <div class="rp-ref-row">
            <label class="field rp-ref-field">
              <span class="field-label">Branch, tag or SHA</span>
              <div class="rp-combo">
                <Input
                  mono
                  value={ref()}
                  onValue={(v) => {
                    setRef(v);
                    setRefOpen(true);
                    setRefHi(0);
                  }}
                  onFocus={() => setRefOpen(true)}
                  // Delay the close so a mouse click on an option still lands.
                  onBlur={() => setTimeout(() => setRefOpen(false), 120)}
                  onKeyDown={onRefKey}
                  placeholder="default branch"
                />
                <Show when={refOpen() && refMatches().length > 0}>
                  <ul class="rp-combo-menu">
                    <For each={refMatches()}>
                      {(r, i) => (
                        <li
                          class={`rp-combo-opt ${i() === refHi() ? "on" : ""}`}
                          onMouseDown={(e) => {
                            e.preventDefault();
                            pickRef(r.name);
                          }}
                          onMouseEnter={() => setRefHi(i())}
                        >
                          <span class={`rp-ref-kind ${r.kind}`}>{r.kind}</span>
                          <span class="rp-ref-name">{r.name}</span>
                          <span class="mono rp-ref-osha">{r.short_sha}</span>
                        </li>
                      )}
                    </For>
                    <li class="rp-combo-hint">…or type a full SHA to pin directly</li>
                  </ul>
                </Show>
              </div>
            </label>
            <button class="btn btn-primary" onClick={loadCatalog}>
              <Icon icon="search" size={14} /> Load pipelines
            </button>
          </div>
          <Show when={catalogValue()}>
            {(c) => (
              <span class="mono rp-sha" title={`resolved to ${c().sha}`}>
                <Icon icon="git-commit-horizontal" size={13} />
                {c().sha.slice(0, 7)}
              </span>
            )}
          </Show>
        </div>
      </div>

      {/* ---- catalog ---- */}
      <div class="panel">
        <div class="panel-h"><span>Pipelines · on: manual</span></div>
        <div class="rp-catalog">
          <Show
            when={loadedRef() !== undefined && !catalog.loading}
            fallback={<p class="empty">loading…</p>}
          >
            <Show
              when={!catalog.error}
              fallback={<p class="empty">Could not read pipelines at this ref.</p>}
            >
              <Show
                when={rows().length > 0}
                fallback={<p class="empty">No manually-dispatchable pipelines at this ref.</p>}
              >
                <For each={rows()}>
                  {(p) => (
                    <button
                      class={`rp-pipe ${selected() === p.name ? "on" : ""} ${p.error ? "err" : ""}`}
                      disabled={!!p.error}
                      onClick={() => !p.error && setSelected(p.name)}
                      title={p.error ?? p.name}
                    >
                      <Icon icon={p.error ? "bug" : "workflow"} size={15} />
                      <span class="mono rp-pipe-name">{p.name}</span>
                      <Show when={p.api}><Pill tone="copper">API</Pill></Show>
                      <Show when={p.error}>
                        <span class="rp-pipe-err">failed to parse — {p.error}</span>
                      </Show>
                    </button>
                  )}
                </For>
              </Show>
            </Show>
          </Show>
        </div>
      </div>

      {/* ---- the picked pipeline's run form ---- */}
      <Show when={selected() && catalogValue()}>
        <RunPipelineDialog
          org={org()}
          repo={repo()}
          pipeline={selected()!}
          gitRef={catalogValue()!.sha}
          refName={loadedRef() === "HEAD" ? null : loadedRef()}
          onClose={() => setSelected(null)}
        />
      </Show>
    </section>
  );
}
