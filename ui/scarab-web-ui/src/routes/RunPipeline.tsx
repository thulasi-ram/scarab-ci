// Run-Pipeline flow (ADR-0043 "World B") — repo → ref → catalog → pipeline →
// typed parameter form → dispatch. One ref threads through; the catalog resolves
// it to a concrete SHA that the interface read and the dispatch pin to, so the
// form and the run see byte-identical config (no branch-moved skew). Client
// validation is static-only (required / type / options); the `validate:` CEL
// predicate runs server-side at dispatch and comes back as a per-field error.
import { createResource, createSignal, createEffect, For, Show } from "solid-js";
import { A, useParams, useNavigate, useLocation } from "@solidjs/router";
import { listPipelines, pipelineInterface, dispatchRun } from "../api/client";
import {
  initialValues,
  reconcilePrefill,
  validateForm,
  toRequestParams,
  mapServerError,
  type FieldValue,
  type ParamSpec,
} from "../params";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";

// Re-run hands the prior run's frozen params through router state (ADR-0043 §6).
// The run-status API carries params but not the pipeline/ref, so the launcher
// re-picks those and the prior params reconcile against whatever interface is
// selected.
type RunPipelineState = { prefillParams?: Record<string, unknown> } | undefined;

export default function RunPipeline() {
  const params = useParams();
  const nav = useNavigate();
  const location = useLocation();
  const org = () => params.org!;
  const repo = () => params.repo!;
  const prefill = (location.state as RunPipelineState)?.prefillParams;

  // The repo default branch (this UI treats `main` as default; HEAD is the
  // server fallback). The chosen ref threads through every call.
  const [ref, setRef] = createSignal("main");
  const [loadedRef, setLoadedRef] = createSignal("main");
  const [selected, setSelected] = createSignal<string | null>(null);

  const [catalog] = createResource(loadedRef, (r) => listPipelines(org(), repo(), r));

  const [iface] = createResource(
    () => {
      const c = catalog();
      const name = selected();
      return c && name ? { name, sha: c.sha } : null;
    },
    (k) => pipelineInterface(org(), repo(), k.name, k.sha),
  );

  // The human catalog: manually-dispatchable pipelines, plus any file that
  // failed to parse (shown disabled/annotated, never crashing the list).
  const rows = () => (catalog()?.pipelines ?? []).filter((p) => p.manual || !!p.error);

  // ---- form state ---------------------------------------------------------
  const [values, setValues] = createSignal<Record<string, FieldValue>>({});
  const [fieldErrors, setFieldErrors] = createSignal<Record<string, string>>({});
  const [formError, setFormError] = createSignal<string | null>(null);
  const [dropped, setDropped] = createSignal<string[]>([]);
  const [submitting, setSubmitting] = createSignal(false);

  // (Re)initialise the form whenever a fresh interface loads. A re-run reconciles
  // the prior params against the current specs (dropping since-removed ones) and
  // surfaces type/validity drift immediately as field errors.
  createEffect(() => {
    const i = iface();
    if (!i) return;
    if (prefill) {
      const { values: v, dropped: d } = reconcilePrefill(i.inputs, prefill);
      setValues(v);
      setDropped(d);
      setFieldErrors(validateForm(i.inputs, v));
    } else {
      setValues(initialValues(i.inputs));
      setDropped([]);
      setFieldErrors({});
    }
    setFormError(null);
  });

  const setField = (name: string, v: FieldValue) => {
    setValues((prev) => ({ ...prev, [name]: v }));
    setFieldErrors((prev) => {
      const next = { ...prev };
      delete next[name];
      return next;
    });
  };

  const loadCatalog = () => {
    setSelected(null);
    setLoadedRef(ref().trim() || "HEAD");
  };

  async function submit() {
    const i = iface();
    const name = selected();
    if (!i || !name) return;
    const errs = validateForm(i.inputs, values());
    if (Object.keys(errs).length > 0) {
      setFieldErrors(errs);
      return;
    }
    setSubmitting(true);
    setFormError(null);
    try {
      const res = await dispatchRun(org(), repo(), {
        ref: i.sha, // pin to the resolved commit the form was built against
        pipeline: name,
        params: toRequestParams(i.inputs, values()),
        kind: "manual",
      });
      if (res.ok) {
        nav(`/${org()}/${repo()}/runs/${res.id}`);
        return;
      }
      const mapped = mapServerError(res.message, i.inputs.map((s) => s.name));
      setFieldErrors(mapped.fieldErrors);
      setFormError(mapped.formError);
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <section class="page">
      <Doodle icon="rocket" size={230} rotate={12} opacity={0.13} top="52px" right="48px" />

      <div class="run-head">
        <h1 class="crumb-head">
          <A href={`/${org()}/${repo()}`} class="crumb-head-link">{repo()}</A>
          <Icon icon="chevron-right" size={20} class="crumb-head-sep" />
          <span class="crumb-head-title">Run pipeline</span>
        </h1>
      </div>

      <Show when={prefill}>
        <p class="subtle rp-rerun-note mono">
          <Icon icon="rotate-cw" size={13} /> Re-run — pre-filled from the prior run's parameters.
        </p>
      </Show>

      {/* ---- ref picker ---- */}
      <div class="panel">
        <div class="panel-h"><span>Ref</span></div>
        <div class="rp-ref">
          <label class="field rp-ref-field">
            <span class="field-label">branch / tag / sha</span>
            <input
              class="input"
              value={ref()}
              onInput={(e) => setRef(e.currentTarget.value)}
              onKeyDown={(e) => e.key === "Enter" && loadCatalog()}
              placeholder="main"
            />
          </label>
          <button class="btn btn-primary" onClick={loadCatalog}>
            <Icon icon="search" size={14} /> Load pipelines
          </button>
          <Show when={catalog()}>
            {(c) => (
              <span class="subtle mono rp-sha" title={c().sha}>
                resolved <b>{c().sha.slice(0, 10)}</b>
              </span>
            )}
          </Show>
        </div>
      </div>

      {/* ---- catalog ---- */}
      <div class="panel">
        <div class="panel-h"><span>Pipelines · on: manual</span></div>
        <div class="rp-catalog">
          <Show when={!catalog.loading} fallback={<p class="empty">loading…</p>}>
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
                      <Show when={p.api}><span class="rp-tag">api</span></Show>
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

      {/* ---- typed parameter form ---- */}
      <Show when={selected()}>
        <div class="panel">
          <div class="panel-h">
            <span>Parameters · {selected()}</span>
            <Show when={iface()}>
              {(i) => <span class="subtle mono">{i().inputs.length} declared</span>}
            </Show>
          </div>
          <div class="rp-form">
            <Show when={!iface.loading} fallback={<p class="empty">compiling interface…</p>}>
              <Show
                when={!iface.error}
                fallback={
                  <p class="error">{(iface.error as Error)?.message ?? "failed to load interface"}</p>
                }
              >
                {(() => {
                  const i = iface()!;
                  return (
                    <>
                      <Show when={dropped().length > 0}>
                        <p class="rp-notice mono">
                          <Icon icon="timer" size={13} /> Dropped {dropped().length} parameter
                          {dropped().length === 1 ? "" : "s"} no longer declared:{" "}
                          <b>{dropped().join(", ")}</b>
                        </p>
                      </Show>
                      <Show
                        when={i.inputs.length > 0}
                        fallback={<p class="subtle">This pipeline declares no launch parameters.</p>}
                      >
                        <For each={i.inputs}>
                          {(spec) => (
                            <Field
                              spec={spec}
                              value={values()[spec.name]}
                              error={fieldErrors()[spec.name]}
                              onChange={(v) => setField(spec.name, v)}
                            />
                          )}
                        </For>
                      </Show>

                      <Show when={formError()}>
                        <p class="error rp-form-error">{formError()}</p>
                      </Show>

                      <div class="rp-actions">
                        <button class="btn btn-primary" disabled={submitting()} onClick={submit}>
                          <Icon icon="play" size={14} /> {submitting() ? "dispatching…" : "Run"}
                        </button>
                        <A class="btn btn-ghost" href={`/${org()}/${repo()}`}>Cancel</A>
                      </div>
                      <p class="subtle rp-hint">
                        <small>
                          Static checks (required · type · options) run here; the pipeline's{" "}
                          <code class="mono">validate:</code> rules run server-side at dispatch.
                        </small>
                      </p>
                    </>
                  );
                })()}
              </Show>
            </Show>
          </div>
        </div>
      </Show>
    </section>
  );
}

// ---- one bounded widget per ParamType (ADR-0043 §2) -----------------------

function Field(props: {
  spec: ParamSpec;
  value: FieldValue | undefined;
  error: string | undefined;
  onChange: (v: FieldValue) => void;
}) {
  const spec = props.spec;
  const strValue = () => (typeof props.value === "string" ? props.value : "");
  const boolValue = () => props.value === true;

  return (
    <div class={`field rp-field ${props.error ? "has-err" : ""}`}>
      <span class="field-label rp-field-label">
        <span class="mono">{spec.name}</span>
        <span class="rp-type">{spec.type}</span>
        <Show when={spec.required} fallback={<span class="rp-opt">optional</span>}>
          <span class="rp-req" title="required">★ required</span>
        </Show>
      </span>

      <Show when={spec.type === "boolean"}>
        <label class="rp-check">
          <input
            type="checkbox"
            checked={boolValue()}
            onChange={(e) => props.onChange(e.currentTarget.checked)}
          />
          <span class="mono">{boolValue() ? "true" : "false"}</span>
        </label>
      </Show>

      <Show when={spec.type === "choice"}>
        <select
          class="input"
          value={strValue()}
          onChange={(e) => props.onChange(e.currentTarget.value)}
        >
          <Show when={spec.required && strValue() === ""}>
            <option value="" disabled>
              select…
            </option>
          </Show>
          <For each={spec.options ?? []}>{(o) => <option value={o}>{o}</option>}</For>
        </select>
      </Show>

      <Show when={spec.type === "number"}>
        <input
          class="input"
          type="number"
          value={strValue()}
          onInput={(e) => props.onChange(e.currentTarget.value)}
          placeholder={spec.required ? "" : String(spec.default ?? "")}
        />
      </Show>

      <Show when={spec.type === "string"}>
        <input
          class="input"
          type="text"
          value={strValue()}
          onInput={(e) => props.onChange(e.currentTarget.value)}
          placeholder={spec.required ? spec.name : String(spec.default ?? "")}
        />
      </Show>

      <Show when={spec.description}>
        <span class="rp-help">{spec.description}</span>
      </Show>
      <Show when={props.error}>
        <span class="rp-err-msg">{props.error}</span>
      </Show>
    </div>
  );
}
