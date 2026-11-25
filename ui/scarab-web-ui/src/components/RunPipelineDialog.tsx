// The run form for one pipeline at one commit, as a dialog. The launcher opens
// it when you pick a pipeline; a run's page opens it filled in with that run's
// inputs ("New run with these inputs").
//
// The interface is read at a ref (the launcher passes the commit its catalog
// resolved; a run's page passes the run's branch, so the new run takes the
// branch's latest commit). The server resolves the ref to a commit, and the
// dispatch pins to that commit, so the form and the run see the same config. Client checks are
// static only (required, type, options); the pipeline's `validate:` rules run
// on the server at dispatch and come back as a field error.
//
// Layout: required inputs first, then the optional ones, both in a two-column
// grid so short controls sit side by side; on/off inputs gather at the end of
// their section. The reason is not a pipeline input, so it sits last, apart.
import { createEffect, createResource, createSignal, For, Show } from "solid-js";
import { useNavigate } from "@solidjs/router";
import { pipelineInterface, dispatchRun, PipelineCompileError } from "../api/client";
import {
  initialValues,
  reconcilePrefill,
  validateForm,
  toRequestParams,
  mapServerError,
  type FieldValue,
  type ParamSpec,
} from "../params";
import Modal, { ModalMessage, ModalSection } from "./Modal";
import CompileErrors from "./CompileErrors";
import { Checkbox, Field, Input } from "./Field";
import SearchSelect from "./SearchSelect";
import Icon from "./Icon";

/** A choice shows as one row of buttons when its options are few and short
 * (this many characters in all); otherwise as a dropdown. */
const SEGMENTED_CHARS = 32;
const segmented = (options: string[]) =>
  options.length > 0 && options.length <= 4 && options.join("").length <= SEGMENTED_CHARS;

export default function RunPipelineDialog(props: {
  org: string;
  repo: string;
  pipeline: string;
  /** The ref the form is read at: a branch, a tag or a commit. The run pins
   * to the commit the server resolves it to. */
  gitRef: string;
  /** What the ref was picked as (a branch, a tag), for the header. */
  refName?: string | null;
  /** A prior run's inputs: the form starts from them, not the defaults. An
   * input whose value no longer passes starts from its default. */
  prefill?: Record<string, unknown>;
  onClose: () => void;
}) {
  const nav = useNavigate();
  const [iface] = createResource(
    () => ({ name: props.pipeline, ref: props.gitRef }),
    (k) => pipelineInterface(props.org, props.repo, k.name, k.ref),
  );
  // Reading an errored resource throws; every read goes through this guard.
  const ifaceValue = () => (iface.error ? undefined : iface());

  const [values, setValues] = createSignal<Record<string, FieldValue>>({});
  const [fieldErrors, setFieldErrors] = createSignal<Record<string, string>>({});
  const [formError, setFormError] = createSignal<string | null>(null);
  const [dropped, setDropped] = createSignal<string[]>([]);
  const [reset, setReset] = createSignal<string[]>([]);
  const [submitting, setSubmitting] = createSignal(false);
  const [reason, setReason] = createSignal("");

  // Start from the prior run's inputs when given (leaving out ones the
  // pipeline no longer declares, and starting ones whose value no longer
  // passes from their default), else from the declared defaults.
  createEffect(() => {
    const i = ifaceValue();
    if (!i) return;
    if (props.prefill) {
      const r = reconcilePrefill(i.inputs, props.prefill);
      setValues(r.values);
      setDropped(r.dropped);
      setReset(r.reset);
    } else {
      setValues(initialValues(i.inputs));
      setDropped([]);
      setReset([]);
    }
    setFieldErrors({});
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

  const required = () => (ifaceValue()?.inputs ?? []).filter((s) => s.required);
  const optional = () => (ifaceValue()?.inputs ?? []).filter((s) => !s.required);
  const canRun = () => !!ifaceValue()?.manual && !submitting();

  async function submit() {
    const i = ifaceValue();
    if (!i) return;
    const errs = validateForm(i.inputs, values());
    if (Object.keys(errs).length > 0) {
      setFieldErrors(errs);
      return;
    }
    setSubmitting(true);
    setFormError(null);
    try {
      const trimmed = reason().trim();
      const res = await dispatchRun(props.org, props.repo, {
        ref: i.sha,
        pipeline: props.pipeline,
        params: toRequestParams(i.inputs, values()),
        kind: "manual",
        ...(trimmed ? { reason: trimmed } : {}),
      });
      if (res.ok) {
        props.onClose();
        nav(`/${props.org}/${props.repo}/runs/${res.id}`);
        return;
      }
      const mapped = mapServerError(res.message, i.inputs.map((s) => s.name));
      setFieldErrors(mapped.fieldErrors);
      setFormError(mapped.formError ?? (Object.keys(mapped.fieldErrors).length > 0 ? "Fix the marked inputs." : null));
    } finally {
      setSubmitting(false);
    }
  }

  // The branch or tag, then the commit it resolved to once the server says.
  const where = () => {
    const label = props.refName ?? (/^[0-9a-f]{40}$/.test(props.gitRef) ? null : props.gitRef);
    const sha = ifaceValue()?.sha.slice(0, 7) ?? (label ? null : props.gitRef.slice(0, 7));
    return [label, sha].filter(Boolean).join(" · ");
  };

  const Grid = (p: { specs: ParamSpec[] }) => {
    const field = (spec: ParamSpec) => (
      <ParamField
        spec={spec}
        value={values()[spec.name]}
        error={fieldErrors()[spec.name]}
        onChange={(v) => setField(spec.name, v)}
      />
    );
    // On/off inputs read as a group on their own rows after the others.
    const fields = () => p.specs.filter((s) => s.type !== "boolean");
    const switches = () => p.specs.filter((s) => s.type === "boolean");
    return (
      <>
        <Show when={fields().length > 0}>
          <div class="rpd-grid">
            <For each={fields()}>{field}</For>
          </div>
        </Show>
        <Show when={switches().length > 0}>
          <div class="rpd-grid">
            <For each={switches()}>{field}</For>
          </div>
        </Show>
      </>
    );
  };

  return (
    <Modal
      size="wide"
      icon="play"
      eyebrow={`${props.org}/${props.repo} · ${where()}`}
      title={`Run ${props.pipeline}`}
      purpose={
        props.prefill
          ? `Filled in with the inputs of the run you came from${props.refName ? `, at the latest commit on ${props.refName}` : ""}.`
          : undefined
      }
      busy={submitting()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={formError()}>{(e) => <ModalMessage>{e()}</ModalMessage>}</Show>
          <span class="sp" />
          <button type="button" class="btn btn-ghost" onClick={props.onClose} disabled={submitting()}>
            Cancel
          </button>
          <button type="button" class="btn btn-primary" onClick={() => void submit()} disabled={!canRun()}>
            <Icon icon="play" size={14} /> {submitting() ? "Starting…" : "Run"}
          </button>
        </>
      }
    >
      <Show when={!iface.loading} fallback={<p class="empty">Reading the pipeline's inputs…</p>}>
        <Show
          when={!iface.error}
          fallback={
            iface.error instanceof PipelineCompileError ? (
              <CompileErrors errors={iface.error.failure.errors} refName={props.refName ?? undefined} sha={iface.error.failure.sha} />
            ) : (
              <p class="error">{(iface.error as Error)?.message ?? "Could not read the pipeline's inputs."}</p>
            )
          }
        >
          <Show when={ifaceValue()}>
            {(i) => (
              <Show
                when={i().manual}
                fallback={
                  <p class="dlg-ctx">
                    This pipeline does not declare <code>on: manual</code> at {where()}, so it cannot be started from
                    here.
                  </p>
                }
              >
                <Show when={dropped().length > 0}>
                  <p class="rpd-notice">
                    <Icon icon="info" size={13} /> Left out {dropped().length === 1 ? "an input" : "inputs"} the pipeline
                    no longer declares: <span class="mono">{dropped().join(", ")}</span>
                  </p>
                </Show>
                <Show when={reset().length > 0}>
                  <p class="rpd-notice">
                    <Icon icon="info" size={13} /> Started from the default, as the earlier value no longer passes:{" "}
                    <span class="mono">{reset().join(", ")}</span>
                  </p>
                </Show>
                <Show when={required().length > 0}>
                  <ModalSection n={1} title="Required">
                    <Grid specs={required()} />
                  </ModalSection>
                </Show>
                <Show when={optional().length > 0}>
                  <ModalSection n={required().length > 0 ? 2 : 1} title="Optional">
                    <Grid specs={optional()} />
                  </ModalSection>
                </Show>
                <Show when={i().inputs.length === 0}>
                  <p class="rpd-none">This pipeline takes no inputs.</p>
                </Show>
                <ModalSection
                  n={(required().length > 0 ? 1 : 0) + (optional().length > 0 ? 1 : 0) + 1}
                  title="Reason"
                >
                  <Field label="Why this run" help="Shown as the run's title. Optional.">
                    <Input value={reason()} onValue={setReason} placeholder="e.g. Re-run after fixing the registry login" />
                  </Field>
                </ModalSection>
              </Show>
            )}
          </Show>
        </Show>
      </Show>
    </Modal>
  );
}

// ---- one control per input type --------------------------------------------

function ParamField(props: {
  spec: ParamSpec;
  value: FieldValue | undefined;
  error: string | undefined;
  onChange: (v: FieldValue) => void;
}) {
  const spec = props.spec;
  const str = () => (typeof props.value === "string" ? props.value : "");
  const label = <span class="mono">{spec.name}</span>;

  if (spec.type === "boolean") {
    return (
      <div class="rpd-cell rpd-bool">
        <Checkbox checked={props.value === true} onChecked={(v) => props.onChange(v)} label={label} help={spec.description} />
      </div>
    );
  }

  const options = spec.options ?? [];
  return (
    <Field class="rpd-cell" label={label} required={spec.required} help={spec.description} error={props.error}>
      <Show when={spec.type === "choice" && segmented(options)}>
        <div class="rpd-seg" role="radiogroup" aria-label={spec.name}>
          <For each={options}>
            {(o) => (
              <button
                type="button"
                role="radio"
                aria-checked={str() === o}
                class="rpd-seg-opt mono"
                classList={{ on: str() === o }}
                onClick={() => props.onChange(o)}
              >
                {o}
              </button>
            )}
          </For>
        </div>
      </Show>
      <Show when={spec.type === "choice" && !segmented(options)}>
        <SearchSelect
          class="ss-block"
          placeholder="Select…"
          searchPlaceholder="Search options…"
          value={str()}
          onChange={(v) => props.onChange(v)}
          options={options.map((o) => ({ value: o, label: o }))}
        />
      </Show>
      <Show when={spec.type === "number"}>
        <Input
          mono
          inputmode="decimal"
          value={str()}
          onValue={(v) => props.onChange(v)}
          placeholder={spec.required ? "" : String(spec.default ?? "")}
        />
      </Show>
      <Show when={spec.type === "string"}>
        <Input
          value={str()}
          onValue={(v) => props.onChange(v)}
          placeholder={spec.required ? "" : String(spec.default ?? "")}
        />
      </Show>
    </Field>
  );
}
