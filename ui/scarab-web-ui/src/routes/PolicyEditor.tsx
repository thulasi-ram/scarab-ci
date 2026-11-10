// One org Policy: `/orgs/<org>/settings/policies/<id>`, or `…/new` for a new
// one. Two tabs: Edit and History (`?tab=history`).
//
// Edit holds the whole Policy: name, checkpoint, what it applies to, mode
// (audit for a new Policy), and its CEL rules. Every rule denies; its
// condition is a CEL expression over the checkpoint's input, and its message
// is fixed text. While you type, the server checks the draft and the page
// shows each finding where it belongs, a rule's at its line and column.
// Field names complete from the checkpoint's input schema. "Try it" runs the
// draft over the newest recorded decisions at its checkpoint and lists the
// ones whose outcome would change, each linking to its Run. Saving a new
// Policy, or a change to one in audit mode, needs no step-up; a change to an
// enforced one asks for a passkey. Delete asks for the name to be typed.
//
// History lists every saved version, newest first, with who saved it, why,
// whether a passkey confirmed it, and a line diff against the version
// before.
//
// Both render inside the settings frame, with Policies marked in the
// sidebar. The server enforces Administer on the org for every write.
import { createEffect, createMemo, createResource, createSignal, For, onCleanup, Show, type JSX } from "solid-js";
import { A, useLocation, useNavigate, useParams, useSearchParams } from "@solidjs/router";
import {
  checkPolicy,
  createPolicy,
  deletePolicy,
  getOrgPolicy,
  listOrgPolicyVersions,
  listPolicyCheckpoints,
  replayPolicy,
  updatePolicy,
  type PolicyCheckResult,
  type PolicyCheckpointInfo,
  type PolicyRecord,
  type PolicyReplayResult,
} from "../api/client";
import {
  applyCompletion,
  completionsAt,
  diagnosticText,
  fieldError,
  hasErrors,
  newPolicyForm,
  placeDiagnostics,
  policyBodyOf,
  policyEditHref,
  policyFormOf,
  policyYaml,
  replayRunHref,
  replaySummary,
  VERDICT_LABEL,
  type CheckpointSchema,
  type Completion,
  type Diagnostic,
  type PolicyForm,
} from "../policy-editor";
import type { PolicyBody } from "../policies";
import { diffStat, lineDiff } from "../line-diff";
import { decodeParam, orgHref } from "../settings-nav";
import { SettingsCrumbs, SettingsFrame } from "./Settings";
import { absTime, relTime } from "../fmt";
import FormSection from "../components/FormSection";
import ConfirmDestructive from "../components/ConfirmDestructive";
import Icon from "../components/Icon";
import { Checkbox, Field, Input, Select } from "../components/Field";

/** The most rules a Policy holds (the server's cap). */
const MAX_RULES = 32;
/** How long the page waits after the last keystroke before checking. */
const CHECK_DELAY_MS = 400;

const CHECKPOINT_WHEN: Record<string, string> = {
  "run.create": "when a Run is created, before any Step exists",
  "step.admit": "at every launch of a Step attempt",
  "gate.resolve": "when someone approves a manual gate",
};

export default function PolicyEditorPage() {
  const params = useParams<{ org: string; id: string }>();
  const org = () => decodeParam(params.org);
  const id = () => decodeParam(params.id);
  const key = createMemo(() => JSON.stringify([org(), id()]));
  return (
    <SettingsFrame kind="org" org={org()} section="policies">
      <Show when={key()} keyed>
        {(_key) => <Editor org={org()} id={id() === "new" ? undefined : id()} />}
      </Show>
    </SettingsFrame>
  );
}

function Editor(props: { org: string; id?: string }) {
  const [search, setSearch] = useSearchParams();
  const tab = () => (props.id && search.tab === "history" ? "history" : "edit");
  const [stored, { refetch }] = createResource(
    () => props.id,
    (id) => getOrgPolicy(props.org, id),
  );
  const title = () => (props.id ? (stored()?.name ?? props.id) : "New policy");

  return (
    <>
      <SettingsCrumbs
        trail={[{ label: "Policies", href: orgHref(props.org, "policies") }, { label: title(), mono: !!props.id }]}
        sub={
          <>
            <span class="mono">{props.org}</span>
            <Show when={stored()}>
              {(p) => (
                <>
                  {" · "}version {p().version} ·{" "}
                  <span title={absTime(p().updated_at)}>changed {relTime(p().updated_at)}</span>
                </>
              )}
            </Show>
          </>
        }
      />
      <Show when={props.id}>
        <div class="tabs">
          <button class={`tab ${tab() === "edit" ? "on" : ""}`} onClick={() => setSearch({ tab: "" })}>
            Edit
          </button>
          <button class={`tab ${tab() === "history" ? "on" : ""}`} onClick={() => setSearch({ tab: "history" })}>
            History
          </button>
        </div>
      </Show>
      <Show when={!stored.error} fallback={<p class="error">Could not load the policy: {String(stored.error?.message ?? "")}</p>}>
        <Show
          when={!props.id || stored()}
          fallback={<p class="empty">loading…</p>}
        >
          <Show
            when={!stored()?.deleted}
            fallback={<p class="empty">This policy is deleted. Its history is kept under History.</p>}
          >
            <Show when={!stored()?.policy || !(stored()!.policy as unknown as PolicyBody).setting}
              fallback={
                <p class="empty">
                  This is a library or plugin list. Edit it on the{" "}
                  <A href={orgHref(props.org, "libraries")}>Libraries</A> or{" "}
                  <A href={orgHref(props.org, "plugins")}>Plugins</A> section.
                </p>
              }
            >
              <Show when={tab() === "edit"}>
                <EditForm org={props.org} stored={stored() ?? undefined} onSaved={() => void refetch()} />
              </Show>
            </Show>
          </Show>
          <Show when={tab() === "history" && props.id}>
            <History org={props.org} id={props.id!} />
          </Show>
        </Show>
      </Show>
    </>
  );
}

function EditForm(props: { org: string; stored?: PolicyRecord; onSaved: () => void }) {
  const nav = useNavigate();
  const location = useLocation<{ draft?: PolicyBody; notice?: string }>();
  const storedBody = () => props.stored?.policy as unknown as PolicyBody | undefined;
  // A pasted Policy arrives as router state on `…/new`.
  const initial = (): PolicyForm => {
    const b = storedBody() ?? location.state?.draft;
    return b ? policyFormOf(b) : newPolicyForm();
  };
  const [form, setForm] = createSignal<PolicyForm>(initial());
  const [touched, setTouched] = createSignal(!!props.stored || !!location.state?.draft);
  const [reason, setReason] = createSignal("");
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [notice, setNotice] = createSignal<string | null>(location.state?.notice ?? null);
  const [deleting, setDeleting] = createSignal(false);
  const [check, setCheck] = createSignal<PolicyCheckResult | null>(null);
  const [checkError, setCheckError] = createSignal<string | null>(null);
  const [replay, setReplay] = createSignal<PolicyReplayResult | null>(null);
  const [replaying, setReplaying] = createSignal(false);
  const [replayError, setReplayError] = createSignal<string | null>(null);
  const [checkpoints] = createResource(() => listPolicyCheckpoints().catch(() => [] as PolicyCheckpointInfo[]));

  const edit = (patch: Partial<PolicyForm>) => {
    setForm((f) => ({ ...f, ...patch }));
    setTouched(true);
    // A result for another draft would mislead.
    setReplay(null);
  };
  const setRule = (i: number, patch: Partial<PolicyForm["rules"][number]>) =>
    edit({ rules: form().rules.map((r, j) => (j === i ? { ...r, ...patch } : r)) });
  const body = () => policyBodyOf(form());
  const checkpoint = () => (checkpoints() ?? []).find((c) => c.checkpoint === form().checkpoint);
  const schema = () => checkpoint()?.schema as unknown as CheckpointSchema | undefined;
  const diagnostics = (): Diagnostic[] => check()?.diagnostics ?? [];
  const placed = () => placeDiagnostics(diagnostics());
  const readsOf = (i: number) => check()?.rules.find((r) => r.index === i)?.reads ?? [];
  const enforced = () => storedBody()?.mode === "enforce";

  // Check the draft a moment after the last change; an answer for an older
  // draft is dropped.
  let seq = 0;
  createEffect(() => {
    const b = body();
    if (!touched()) return;
    const mine = ++seq;
    const t = setTimeout(() => {
      checkPolicy(props.org, { policy: b })
        .then((r) => {
          if (mine === seq) {
            setCheck(r);
            setCheckError(null);
          }
        })
        .catch((e) => {
          if (mine === seq) setCheckError(e instanceof Error ? e.message : "The check failed.");
        });
    }, CHECK_DELAY_MS);
    onCleanup(() => clearTimeout(t));
  });

  const save = async () => {
    if (saving()) return;
    setSaving(true);
    setError(null);
    setNotice(null);
    try {
      if (props.stored) {
        const saved = await updatePolicy({ org: props.org }, props.stored.id, body(), props.stored.version, reason());
        setReason("");
        setNotice(`Saved as version ${saved.version}.`);
        props.onSaved();
      } else {
        const created = await createPolicy({ org: props.org }, body(), reason());
        nav(policyEditHref(props.org, created.id), {
          state: { notice: `${created.name} is added${body().mode === "audit" ? " in audit mode" : ""}.` },
        });
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : "The server refused it.");
    } finally {
      setSaving(false);
    }
  };

  const tryIt = async () => {
    setReplaying(true);
    setReplayError(null);
    try {
      setReplay(await replayPolicy(props.org, body(), props.stored?.id));
    } catch (e) {
      setReplayError(e instanceof Error ? e.message : "Could not try the policy.");
      setReplay(null);
    } finally {
      setReplaying(false);
    }
  };

  const copyYaml = async () => {
    try {
      await navigator.clipboard.writeText(policyYaml(body()));
      setNotice("The policy is copied as YAML.");
    } catch {
      setNotice("Could not copy: the YAML is under Preview YAML below.");
    }
  };

  const canSave = () => touched() && !saving() && !!check() && !hasErrors(diagnostics());

  return (
    <div class="env-edit pol-edit">
      <Show when={notice()}>
        <p class="notice-ok">{notice()}</p>
      </Show>
      <div class="panel">
        <div class="panel-h">
          <span>Policy</span>
          <span class="oenv-actions">
            <button class="btn btn-ghost btn-sm" type="button" onClick={copyYaml}>
              <Icon icon="copy" size={13} /> Copy YAML
            </button>
          </span>
        </div>
        <div class="secrets-body token-form">
          <Show when={placed().general.length > 0 || checkError()}>
            <ul class="pol-diags" aria-label="Problems with the policy">
              <Show when={checkError()}>
                <li class="pol-diag-error">{checkError()}</li>
              </Show>
              <For each={placed().general}>{(d) => <DiagnosticLine d={d} />}</For>
            </ul>
          </Show>

          <FormSection n={1} title="Policy">
            <Field
              label="Name"
              required
              error={fieldError(placed().fields.name)}
              help="Letters, digits, -, _ or . Decisions and refusals show it."
            >
              <Input mono value={form().name} onValue={(v) => edit({ name: v })} />
            </Field>
            <Field
              label="Checkpoint"
              error={fieldError(placed().fields.checkpoint)}
              help={CHECKPOINT_WHEN[form().checkpoint] ?? ""}
            >
              <Select mono value={form().checkpoint} onValue={(v) => edit({ checkpoint: v })}>
                <For each={checkpoints() ?? [{ checkpoint: form().checkpoint } as PolicyCheckpointInfo]}>
                  {(c) => (
                    // `selected` keeps the choice when the options load after the form.
                    <option value={c.checkpoint} selected={c.checkpoint === form().checkpoint}>
                      {c.checkpoint}
                    </option>
                  )}
                </For>
              </Select>
            </Field>
          </FormSection>

          <FormSection n={2} title="Applies to" note="Empty lists match everything. * matches any run of characters.">
            <Field label="Projects" error={fieldError(placed().fields.projects)} help="Comma-separated. Empty: every project.">
              <Input mono placeholder="every project" value={form().projects} onValue={(v) => edit({ projects: v })} />
            </Field>
            <Field
              label="Except projects"
              error={fieldError(placed().fields.exceptProjects)}
              help="Projects it never applies to, even when they match above."
            >
              <Input mono value={form().exceptProjects} onValue={(v) => edit({ exceptProjects: v })} />
            </Field>
            <Field
              label="Environments"
              error={fieldError(placed().fields.environments)}
              help="Runs whose pipeline names one of these. Empty: every Run, with an Environment or without."
            >
              <Input mono placeholder="any" value={form().environments} onValue={(v) => edit({ environments: v })} />
            </Field>
            <Show when={(checkpoint()?.trigger_kinds ?? []).length > 0}>
              <div class="f">
                <div class="f-l">
                  <span class="f-lbl">Trigger kinds</span>
                </div>
                <div class="pol-kinds">
                  <For each={checkpoint()!.trigger_kinds}>
                    {(k) => (
                      <Checkbox
                        checked={form().triggerKinds.includes(k)}
                        onChecked={(on) =>
                          edit({
                            triggerKinds: on
                              ? [...form().triggerKinds, k]
                              : form().triggerKinds.filter((x) => x !== k),
                          })
                        }
                        label={<span class="mono">{k}</span>}
                      />
                    )}
                  </For>
                </div>
                <Show
                  when={fieldError(placed().fields.triggerKinds)}
                  fallback={<p class="f-help">None checked: every trigger.</p>}
                >
                  <p class="f-err">{fieldError(placed().fields.triggerKinds)}</p>
                </Show>
              </div>
            </Show>
          </FormSection>

          <FormSection n={3} title="Mode">
            <Field
              label="Mode"
              help={
                form().mode === "audit"
                  ? "Audit: each decision records what this policy would deny, and nothing is denied."
                  : "Enforce: a Run or Step this policy matches and a rule holds for is refused."
              }
            >
              <Select value={form().mode} onValue={(v) => edit({ mode: v as PolicyForm["mode"] })}>
                <option value="audit">audit</option>
                <option value="enforce">enforce</option>
              </Select>
            </Field>
          </FormSection>

          <FormSection
            n={4}
            title="Rules"
            note="A rule denies when its condition holds. Its message is shown with the refusal and on the Run page."
          >
            <For each={form().rules}>
              {(rule, i) => (
                <div class="pol-rule">
                  <div class="pol-rule-h">
                    <span>Rule {i() + 1}</span>
                    <span class="tok-role">deny</span>
                    <Show when={form().rules.length > 1}>
                      <button
                        class="btn btn-ghost btn-sm oenv-actions"
                        type="button"
                        onClick={() => edit({ rules: form().rules.filter((_, j) => j !== i()) })}
                      >
                        Remove
                      </button>
                    </Show>
                  </div>
                  <div class="f">
                    <div class="f-l">
                      <label class="f-lbl" for={`pol-when-${i()}`}>
                        Condition
                      </label>
                      <span class="f-req" aria-hidden="true">*</span>
                    </div>
                    <CelInput
                      id={`pol-when-${i()}`}
                      value={rule.when}
                      onValue={(v) => setRule(i(), { when: v })}
                      schema={schema()}
                      invalid={(placed().rules.get(i())?.when ?? []).some((d) => d.severity === "error")}
                    />
                    <Show
                      when={(placed().rules.get(i())?.when ?? []).length > 0}
                      fallback={
                        <p class="f-help">
                          <Show when={readsOf(i()).length > 0} fallback="A CEL expression over the checkpoint's input.">
                            Reads <span class="mono">{readsOf(i()).join(", ")}</span>.
                          </Show>
                        </p>
                      }
                    >
                      <ul class="pol-diags" aria-label={`Problems with rule ${i() + 1}`}>
                        <For each={placed().rules.get(i())!.when}>{(d) => <DiagnosticLine d={d} />}</For>
                      </ul>
                    </Show>
                  </div>
                  <Field label="Message" required error={fieldError(placed().rules.get(i())?.message)}>
                    <Input value={rule.message} onValue={(v) => setRule(i(), { message: v })} />
                  </Field>
                </div>
              )}
            </For>
            <div>
              <button
                class="btn btn-ghost btn-sm"
                type="button"
                disabled={form().rules.length >= MAX_RULES}
                onClick={() => edit({ rules: [...form().rules, { when: "", message: "" }] })}
              >
                <Icon icon="plus" size={14} /> Add rule
              </button>
            </div>
          </FormSection>

          <FormSection
            n={5}
            title="Try it"
            note="Runs this draft over the newest recorded decisions at its checkpoint, without enforcing or recording anything, and lists the ones whose outcome would change."
          >
            <div>
              <button
                class="btn btn-ghost btn-sm"
                type="button"
                disabled={replaying() || !check() || hasErrors(diagnostics())}
                onClick={tryIt}
              >
                <Icon icon="play" size={13} /> {replaying() ? "Trying…" : "Try it on recent decisions"}
              </button>
            </div>
            <Show when={replayError()}>
              <p class="error">{replayError()}</p>
            </Show>
            <Show when={replay()}>{(r) => <ReplayResult org={props.org} result={r()} />}</Show>
          </FormSection>

          <FormSection n={6} title="Save">
            <Field label="Reason" help="Recorded with this version in the history.">
              <Input value={reason()} onValue={setReason} />
            </Field>
            <details class="pol-yaml">
              <summary>Preview YAML</summary>
              <pre class="mono">{policyYaml(body())}</pre>
            </details>
          </FormSection>
        </div>
        <div class="modal-foot">
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <Show when={touched() && hasErrors(diagnostics())}>
            <p class="subtle">Fix the problems above to save.</p>
          </Show>
          <Show when={props.stored && enforced()}>
            <p class="subtle">
              <Icon icon="shield-check" size={12} /> This policy is enforced: saving a change asks for a passkey.
            </p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={!canSave()} onClick={save}>
              {saving() ? "Saving…" : props.stored ? "Save" : "Add policy"}
            </button>
            <A href={orgHref(props.org, "policies")} class="btn btn-ghost">
              Cancel
            </A>
          </div>
        </div>
      </div>

      <Show when={props.stored}>
        {(p) => (
          <div class="panel env-danger">
            <div class="panel-h">
              <span>Delete</span>
            </div>
            <div class="secrets-body">
              <p>
                It stops being consulted; its history stays.
                {enforced() ? " Deleting an enforced policy asks for a passkey." : ""}
              </p>
              <div>
                <button class="btn btn-danger btn-sm" onClick={() => setDeleting(true)}>
                  Delete policy
                </button>
              </div>
              <Show when={deleting()}>
                <ConfirmDestructive
                  eyebrow={`${props.org} · policies`}
                  title={`Delete ${p().name}?`}
                  purpose={
                    enforced()
                      ? "Runs and Steps it denies are admitted again."
                      : "It is in audit mode, so nothing it enforces stops."
                  }
                  name={p().name}
                  confirmLabel="Delete policy"
                  onConfirm={async () => {
                    await deletePolicy({ org: props.org }, p().id, p().version);
                    nav(orgHref(props.org, "policies"));
                  }}
                  onClose={() => setDeleting(false)}
                />
              </Show>
            </div>
          </div>
        )}
      </Show>
    </div>
  );
}

function DiagnosticLine(props: { d: Diagnostic }) {
  return (
    <li class={props.d.severity === "error" ? "pol-diag-error" : "pol-diag-warn"}>
      <Icon icon={props.d.severity === "error" ? "alert-triangle" : "circle-dot"} size={12} />
      <span>{diagnosticText(props.d)}</span>
    </li>
  );
}

/**
 * A rule's condition: a monospace textarea with field-name completion from
 * the checkpoint's input schema. Tab or Enter takes the highlighted name,
 * the arrows move it, Escape closes the list; a click takes one too.
 */
function CelInput(props: {
  id: string;
  value: string;
  onValue: (v: string) => void;
  schema: CheckpointSchema | undefined;
  invalid: boolean;
}) {
  let ta: HTMLTextAreaElement | undefined;
  const [cursor, setCursor] = createSignal<number | null>(null);
  const [active, setActive] = createSignal(0);
  const [closed, setClosed] = createSignal(false);
  const found = createMemo(() => {
    const c = cursor();
    if (!props.schema || c === null || closed()) return { from: 0, items: [] as Completion[] };
    return completionsAt(props.schema, props.value, c);
  });
  const items = () => found().items.slice(0, 8);
  const track = () => {
    setCursor(ta?.selectionStart ?? null);
    setActive(0);
  };
  const pick = (c: Completion) => {
    const at = cursor();
    if (at === null) return;
    const [text, next] = applyCompletion(props.value, found().from, at, c.label);
    props.onValue(text);
    queueMicrotask(() => {
      ta?.focus();
      ta?.setSelectionRange(next, next);
      setCursor(next);
    });
  };
  const onKey: JSX.EventHandler<HTMLTextAreaElement, KeyboardEvent> = (e) => {
    const list = items();
    if (list.length === 0) return;
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      setActive((a) => (a + (e.key === "ArrowDown" ? 1 : list.length - 1)) % list.length);
    } else if (e.key === "Tab" || e.key === "Enter") {
      e.preventDefault();
      pick(list[active()]!);
    } else if (e.key === "Escape") {
      setClosed(true);
    }
  };
  return (
    <div class="cel-input">
      <div class={`f-ctl f-area${props.invalid ? " is-err" : ""}`}>
        <textarea
          ref={ta}
          id={props.id}
          class="f-inp mono"
          rows={3}
          spellcheck={false}
          value={props.value}
          placeholder="steps.exists(s, s.deploy)"
          aria-invalid={props.invalid || undefined}
          aria-autocomplete="list"
          onInput={(e) => {
            setClosed(false);
            props.onValue(e.currentTarget.value);
            track();
          }}
          onKeyDown={onKey}
          onKeyUp={(e) => {
            if (e.key.startsWith("Arrow") && items().length > 0 && (e.key === "ArrowDown" || e.key === "ArrowUp")) return;
            if (e.key !== "Escape") track();
          }}
          onClick={track}
          onBlur={() => setCursor(null)}
        />
      </div>
      <Show when={items().length > 0}>
        <ul class="cel-complete" role="listbox" aria-label="Field names">
          <For each={items()}>
            {(c, i) => (
              <li
                role="option"
                aria-selected={i() === active()}
                classList={{ on: i() === active() }}
                // Taken on mousedown, before the textarea's blur clears the list.
                onMouseDown={(e) => {
                  e.preventDefault();
                  pick(c);
                }}
              >
                <span class="mono">{c.label}</span>
                <span class="subtle mono">{c.detail}</span>
                <Show when={c.optional}>
                  <span class="cel-opt" title="It may be absent: test it with has() first">
                    optional
                  </span>
                </Show>
                <Show when={c.doc}>
                  <span class="cel-doc">{c.doc}</span>
                </Show>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </div>
  );
}

function ReplayResult(props: { org: string; result: PolicyReplayResult }) {
  return (
    <div class="pol-replay">
      <p class="subtle">{replaySummary(props.result)}</p>
      <Show when={props.result.changed.length > 0}>
        <ul class="tok-list" aria-label="Decisions that would change">
          <For each={props.result.changed}>
            {(c) => (
              <li class="tok">
                <div class="tok-head">
                  <span class="mono">{VERDICT_LABEL[c.before] ?? c.before}</span>
                  <Icon icon="chevron-right" size={12} />
                  <span class={`tok-role ${c.after === "deny" ? "pol-denied" : ""}`}>
                    {VERDICT_LABEL[c.after] ?? c.after}
                  </span>
                  <Show when={c.project}>
                    <span class="mono">{c.project}</span>
                  </Show>
                  <Show when={c.step_id}>
                    <span class="mono subtle">
                      {c.step_id}
                      <Show when={c.attempt}> · try {c.attempt}</Show>
                    </span>
                  </Show>
                  <Show
                    when={replayRunHref(props.org, c)}
                    fallback={<span class="subtle">refused Run</span>}
                  >
                    {(href) => <A href={href()}>run</A>}
                  </Show>
                  <span class="subtle" title={absTime(c.at)}>
                    {relTime(c.at)}
                  </span>
                </div>
                <Show when={c.messages.length > 0}>
                  <div class="tok-facts">{c.messages.join(" · ")}</div>
                </Show>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </div>
  );
}

function History(props: { org: string; id: string }) {
  const [versions] = createResource(() => listOrgPolicyVersions(props.org, props.id));
  // Newest first, each with the version before it.
  const rows = () => {
    const vs = versions() ?? [];
    return vs
      .map((v, i) => ({ v, prev: i > 0 ? vs[i - 1] : undefined }))
      .reverse();
  };
  const yamlOf = (p: unknown) => policyYaml(p as PolicyBody);
  return (
    <div class="panel">
      <div class="panel-h">
        <span>History</span>
        <Show when={(versions() ?? []).length > 0}>
          <span class="tok-count">{versions()!.length}</span>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={!versions.error} fallback={<p class="error">Could not load the history.</p>}>
          <Show when={versions()} fallback={<p class="empty">loading…</p>}>
            <ul class="tok-list" aria-label="Versions">
              <For each={rows()}>
                {({ v, prev }, i) => {
                  const diff = () => lineDiff(prev ? yamlOf(prev.policy) : "", v.deleted ? "" : yamlOf(v.policy));
                  return (
                    <li class="tok">
                      <div class="tok-head">
                        <span class="tok-name mono">version {v.version}</span>
                        <Show when={v.deleted}>
                          <span class="tok-role pol-denied">deleted</span>
                        </Show>
                        <Show when={v.step_up_ref}>
                          <span class="tok-role" title={`step-up ${v.step_up_ref}`}>
                            confirmed with a passkey
                          </span>
                        </Show>
                        <span class="mono subtle">{v.author}</span>
                        <span class="subtle" title={absTime(v.at)}>
                          {relTime(v.at)}
                        </span>
                      </div>
                      <Show when={v.reason}>
                        <div class="tok-facts">{v.reason}</div>
                      </Show>
                      <details class="pol-diff" open={i() === 0}>
                        <summary>
                          {prev ? `Changes from version ${prev.version}` : "The first version"}
                          {" · "}+{diffStat(diff()).added} −{diffStat(diff()).removed}
                        </summary>
                        <pre class="mono">
                          <For each={diff()}>
                            {(l) => (
                              <span class={`pol-diff-${l.kind}`}>
                                {l.kind === "add" ? "+ " : l.kind === "del" ? "- " : "  "}
                                {l.text}
                                {"\n"}
                              </span>
                            )}
                          </For>
                        </pre>
                      </details>
                    </li>
                  );
                }}
              </For>
            </ul>
          </Show>
        </Show>
      </div>
    </div>
  );
}
