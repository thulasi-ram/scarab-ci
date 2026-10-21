// Org-level Environments (ADR-0071, MMP step 1) — the Settings section where an
// org admin registers a place deployments land in: its name, how long a
// Deployment there may take before it reads `stalled` (the
// EnvironmentDeploymentDeadline), and which sources may report into it (the
// EnvironmentDeploymentSources).
//
// Additive, and the panel says so: per-repo Environments on a repository's
// Environments tab still decide who may deploy (approvers, allowed refs). An
// org Environment shares their name and nothing else yet. A name that has
// Deployments but no org Environment still works — it gets the default
// deadline and no routed sources — so registering one is an improvement, never
// a prerequisite.
//
// Saving is a FULL replace (`toPutBody` always sends every field), and the
// server's 400/409 text is shown as-is: "already reports into environment
// `prod`" is the whole answer to why a source will not save.
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import {
  deleteOrgEnvironment,
  listOrgEnvironments,
  putOrgEnvironment,
  type OrgEnvironment,
} from "../api/client";
import {
  deadlineLabel,
  MAX_DEADLINE_MINUTES,
  nameError,
  toForm,
  toPutBody,
  type SourceRow,
} from "../org-environments";
import { relTime, absTime } from "../fmt";
import Icon from "./Icon";

export default function OrgEnvironments(props: { org: string }) {
  const [epoch, setEpoch] = createSignal(0);
  const [environments] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listOrgEnvironments(k.org),
  );
  /** `undefined` = closed; `null` = creating; an Environment = editing it. */
  const [editing, setEditing] = createSignal<OrgEnvironment | null | undefined>(undefined);
  const [error, setError] = createSignal<string | null>(null);
  const reload = () => setEpoch((e) => e + 1);

  // `null` is the server answering "no org-Environment store here", which is a
  // different sentence from "none registered".
  const unavailable = () => !environments.loading && !environments.error && environments() === null;
  const rows = () => environments() ?? [];

  const remove = async (env: OrgEnvironment) => {
    if (
      !window.confirm(
        `Delete the ${env.name} environment?\n\nIts deadline and sources go with it. Deployments named ${env.name} would fall back to the default deadline — which is why the server refuses this once ${env.name} has any.`,
      )
    )
      return;
    setError(null);
    try {
      await deleteOrgEnvironment(props.org, env.name);
      reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : `Could not delete ${env.name}.`);
    }
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>Environments · {props.org}</span>
        <Show when={rows().length > 0}>
          <span class="tok-count">{rows().length}</span>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={environments.error}>
          <p class="error">Could not load the org's environments.</p>
        </Show>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>

        <Show when={!environments.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={!unavailable()}
            fallback={<p class="empty">This server does not keep org-level environments.</p>}
          >
            <Show
              when={rows().length > 0}
              fallback={
                <p class="empty">
                  No environments registered yet. Deployments still show under whatever
                  environment name they carry; registering one sets how long a deployment
                  there may take, and which sources may report into it.
                </p>
              }
            >
              <ul class="tok-list">
                <For each={rows()}>
                  {(env) => (
                    <EnvironmentRow env={env} onEdit={() => setEditing(env)} onDelete={() => remove(env)} />
                  )}
                </For>
              </ul>
            </Show>

            <div class="secrets-actions">
              <button class="btn btn-primary btn-sm" onClick={() => setEditing(null)}>
                <Icon icon="plus" size={14} /> New environment
              </button>
            </div>
            <p class="subtle">
              <small>
                <Icon icon="rocket" size={12} /> An org environment is the place deployments
                land in, shared by every repository that deploys there. Who may deploy is
                still each repository's own Environments tab.
              </small>
            </p>
          </Show>
        </Show>
      </div>

      <Show when={editing() !== undefined}>
        <EnvironmentDialog
          org={props.org}
          env={editing() ?? undefined}
          taken={rows().map((e) => e.name)}
          onClose={() => setEditing(undefined)}
          onSaved={() => {
            setEditing(undefined);
            reload();
          }}
        />
      </Show>
    </div>
  );
}

/** One registered Environment: name, deadline, sources, and who last changed it. */
function EnvironmentRow(props: { env: OrgEnvironment; onEdit: () => void; onDelete: () => void }) {
  const env = () => props.env;
  return (
    <li class="tok">
      <div class="tok-head">
        <Icon icon="rocket" size={15} />
        <span class="tok-name mono">{env().name}</span>
        <span class="tok-role" title="EnvironmentDeploymentDeadline: past this, an unfinished deployment reads stalled">
          stalls after {deadlineLabel(env().deployment_deadline_ms)}
        </span>
        <span class="oenv-actions">
          <button class="btn btn-ghost btn-sm" onClick={props.onEdit}>
            edit
          </button>
          <button class="btn btn-danger btn-sm" onClick={props.onDelete}>
            delete
          </button>
        </span>
      </div>
      <Show when={env().description}>
        <div class="oenv-desc">{env().description}</div>
      </Show>
      <div class="tok-facts">
        <span>
          <Icon icon="waypoints" size={12} />
          <Show when={env().deployment_sources.length > 0} fallback={"no sources may report here"}>
            <span class="oenv-sources">
              <For each={env().deployment_sources}>
                {(s) => (
                  <span class="mono" title="PluginInstallation / vendor grouping">
                    {s.installation} / {s.vendor_grouping}
                  </span>
                )}
              </For>
            </span>
          </Show>
        </span>
        <span title={absTime(env().updated_at)}>
          <Icon icon="user" size={12} />
          {env().updated_by} · {relTime(env().updated_at)}
        </span>
      </div>
    </li>
  );
}

/** Create (no `env`) or edit an Environment. The name is fixed once it exists:
 *  it is the key Deployments and per-repo Environments share. */
function EnvironmentDialog(props: {
  org: string;
  env?: OrgEnvironment;
  /** Names already registered — a create must not silently replace one. */
  taken: string[];
  onClose: () => void;
  onSaved: () => void;
}) {
  const initial = toForm(props.env);
  const [name, setName] = createSignal(props.env?.name ?? "");
  const [description, setDescription] = createSignal(initial.description);
  const [minutes, setMinutes] = createSignal(initial.deadlineMinutes);
  const [sources, setSources] = createSignal<SourceRow[]>(
    initial.sources.length > 0 ? initial.sources : [{ installation: "", vendor_grouping: "" }],
  );
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") props.onClose();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  const creating = () => !props.env;
  const nameProblem = () => {
    if (!creating()) return null;
    const n = name().trim();
    if (props.taken.includes(n)) return `${n} is already registered — edit it instead.`;
    return nameError(n);
  };
  const built = () =>
    toPutBody({ description: description(), deadlineMinutes: minutes(), sources: sources() });
  const problem = () => nameProblem() ?? built().error ?? null;

  const setRow = (i: number, patch: Partial<SourceRow>) =>
    setSources((rows) => rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));

  const save = async () => {
    const body = built().body;
    if (!body || problem() || saving()) return;
    setSaving(true);
    setError(null);
    try {
      await putOrgEnvironment(props.org, creating() ? name().trim() : props.env!.name, body);
      props.onSaved();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save the environment.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal modal-tall" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h">
          <span>{creating() ? "New environment" : `Edit ${props.env!.name}`}</span>
        </div>
        <div class="modal-body token-form">
          <Show when={creating()}>
            <div class="form-r">
              <label>name</label>
              <input
                class="input"
                placeholder="production, staging, prod-eu"
                value={name()}
                onInput={(e) => setName(e.currentTarget.value)}
                autofocus
              />
              <p class="fieldnote">
                Exactly the name pipelines and deployments use — case matters. It cannot be
                changed later.
              </p>
            </div>
          </Show>

          <div class="form-r">
            <label>description</label>
            <input
              class="input"
              placeholder="what lands here, who runs it"
              value={description()}
              onInput={(e) => setDescription(e.currentTarget.value)}
            />
          </div>

          <div class="form-r">
            <label>deployment deadline</label>
            <div class="tok-life">
              <input
                class="input tok-days"
                type="number"
                min="1"
                max={MAX_DEADLINE_MINUTES}
                value={minutes()}
                onInput={(e) => setMinutes(Number(e.currentTarget.value))}
              />
              <span class="tok-life-unit">minutes</span>
              <For each={[15, 60, 240]}>
                {(m) => (
                  <button
                    class="btn btn-ghost btn-sm"
                    classList={{ "tok-preset-on": minutes() === m }}
                    onClick={() => setMinutes(m)}
                  >
                    {deadlineLabel(m * 60_000)}
                  </button>
                )}
              </For>
            </div>
            <p class="fieldnote">
              A deployment still requested or in progress after this long reads{" "}
              <b>stalled</b>. 1 minute to 7 days; set it above your slowest healthy rollout,
              retries included.
            </p>
          </div>

          <div class="form-r">
            <label>deployment sources</label>
            <div class="oenv-rows">
              <For each={sources()}>
                {(row, i) => (
                  <div class="oenv-row">
                    <input
                      class="input mono"
                      placeholder="installation (argocd-prod)"
                      value={row.installation}
                      onInput={(e) => setRow(i(), { installation: e.currentTarget.value })}
                    />
                    <input
                      class="input mono"
                      placeholder="vendor grouping (Argo project)"
                      value={row.vendor_grouping}
                      onInput={(e) => setRow(i(), { vendor_grouping: e.currentTarget.value })}
                    />
                    <button
                      class="btn btn-ghost btn-sm"
                      title="Remove this source"
                      onClick={() => setSources((rows) => rows.filter((_, j) => j !== i()))}
                    >
                      ✕
                    </button>
                  </div>
                )}
              </For>
              <div>
                <button
                  class="btn btn-ghost btn-sm"
                  onClick={() => setSources((rows) => [...rows, { installation: "", vendor_grouping: "" }])}
                >
                  <Icon icon="plus" size={12} /> add source
                </button>
              </div>
            </div>
            <p class="fieldnote">
              Which plugin installation may report into this environment, and for which of
              its groupings (an Argo CD project, say) — matched exactly. A source reports
              into one environment only. Reports from anything not listed here will be
              dropped once source reporting arrives.
            </p>
          </div>
        </div>

        <div class="modal-foot">
          <Show when={problem()}>
            <p class="error">{problem()}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={!!problem() || saving()} onClick={save}>
              {saving() ? "Saving…" : creating() ? "Create environment" : "Save"}
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
