// Org-level Environments (ADR-0071) — the Settings section where an org admin
// runs the places deploys land in. Each Environment has:
//
//   - its description (the whole-Environment form, a FULL replace: `toPutBody`
//     always sends every field);
//   - its BASELINE protection rules, which every project's Deploy Runs obey;
//   - its ELIGIBILITY — all projects, or project-name patterns. An empty list
//     admits no project: eligibility starts closed;
//   - per-project PRIVILEGE GRANTS (image digests, raw k8s_overlay) — granted
//     to one project, never to the Environment as a whole.
//
// Each of the last three has its own write, so the description form can never
// reset them. Projects tighten the baseline themselves (their overlay, on the
// repository's Environments tab); this page shows those overlays read-only.
//
// Above the list sits the migration's SPLIT REPORT: every name the migration
// merged across projects, or opened to all projects, with each project's old
// rules beside its overlay now, and a banner counting what is left to review
// and which baselines are still permissive. Splitting a name that is really
// two places is a YAML change in the projects concerned — the panel says so.
//
// Every server refusal (400/409) is shown verbatim: it is the answer to "why
// will this not save".
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { Portal } from "solid-js/web";
import {
  acknowledgeMigrationReport,
  deleteOrgEnvironment,
  deletePrivilegeGrant,
  getMigrationReport,
  listEnvironmentProjects,
  listOrgEnvironments,
  putEnvironmentEligibility,
  putEnvironmentProtection,
  putOrgEnvironment,
  putPrivilegeGrant,
  type MigrationReportLine,
  type OrgEnvironment,
} from "../api/client";
import {
  eligibilityLabel,
  isPermissive,
  reportBanner,
  rulesSummary,
  rulesToForm,
  toEligibilityBody,
  toGrantBody,
  toProtectionBody,
  type ImageGrantRow,
} from "../environment-overlays";
import PrincipalSearch from "./PrincipalSearch";
import { addToList } from "../identities";
import { nameError, toForm, toPutBody } from "../org-environments";
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
  /** The governance dialog open, if any: which Environment, which part. */
  const [governing, setGoverning] = createSignal<
    { env: OrgEnvironment; part: "baseline" | "eligibility" | "grants" } | undefined
  >(undefined);
  const [report, { refetch: refetchReport }] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => getMigrationReport(k.org).catch(() => null),
  );
  const [error, setError] = createSignal<string | null>(null);
  const reload = () => setEpoch((e) => e + 1);
  const govern = (env: OrgEnvironment, part: "baseline" | "eligibility" | "grants") =>
    setGoverning({ env, part });

  // `null` is the server answering "no org-Environment store here", which is a
  // different sentence from "none registered".
  const unavailable = () => !environments.loading && !environments.error && environments() === null;
  const rows = () => environments() ?? [];

  const remove = async (env: OrgEnvironment) => {
    if (
      !window.confirm(
        `Delete the ${env.name} environment?\n\nPipelines naming ${env.name} are refused until it exists again.`,
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

        <Show when={(report() ?? []).length > 0}>
          <MigrationReport org={props.org} lines={report()!} onChanged={() => refetchReport()} />
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
                  No environments yet. A pipeline's <span class="mono">environment:</span>{" "}
                  is refused until an environment of that name exists here and lets the
                  project deploy into it.
                </p>
              }
            >
              <ul class="tok-list">
                <For each={rows()}>
                  {(env) => (
                    <EnvironmentRow
                      env={env}
                      onEdit={() => setEditing(env)}
                      onDelete={() => remove(env)}
                      onGovern={(part) => govern(env, part)}
                    />
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
                <Icon icon="rocket" size={12} /> An org environment is the place deploys
                land in, shared by every repository that deploys there. It decides who may
                deploy (eligibility) and under which rules (the baseline); each repository
                can only tighten those for itself.
              </small>
            </p>
          </Show>
        </Show>
      </div>

      <Show when={editing() !== undefined}>
        {/* Portalled, as Plugins' install dialog: in-panel, it rendered under
            the panels after this one (each is its own stacking context). */}
        <Portal>
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
        </Portal>
      </Show>

      <Show when={governing()}>
        {(g) => (
          <Portal>
            <Show when={g().part === "baseline"}>
              <BaselineDialog
                org={props.org}
                env={g().env}
                onClose={() => setGoverning(undefined)}
                onSaved={() => {
                  setGoverning(undefined);
                  reload();
                }}
              />
            </Show>
            <Show when={g().part === "eligibility"}>
              <EligibilityDialog
                org={props.org}
                env={g().env}
                onClose={() => setGoverning(undefined)}
                onSaved={() => {
                  setGoverning(undefined);
                  reload();
                }}
              />
            </Show>
            <Show when={g().part === "grants"}>
              <ProjectsDialog org={props.org} env={g().env} onClose={() => setGoverning(undefined)} />
            </Show>
          </Portal>
        )}
      </Show>
    </div>
  );
}

/** One registered Environment: name, rules, and who last changed it. */
function EnvironmentRow(props: {
  env: OrgEnvironment;
  onEdit: () => void;
  onDelete: () => void;
  onGovern: (part: "baseline" | "eligibility" | "grants") => void;
}) {
  const env = () => props.env;
  return (
    <li class="tok">
      <div class="tok-head">
        <Icon icon="rocket" size={15} />
        <span class="tok-name mono">{env().name}</span>
        <Show when={isPermissive(env().protection)}>
          <span class="tok-role" title="No org-wide rule protects this place yet — only each project's own overlay">
            permissive baseline
          </span>
        </Show>
        <span class="oenv-actions">
          <button class="btn btn-ghost btn-sm" onClick={() => props.onGovern("baseline")}>
            baseline
          </button>
          <button class="btn btn-ghost btn-sm" onClick={() => props.onGovern("eligibility")}>
            eligibility
          </button>
          <button class="btn btn-ghost btn-sm" onClick={() => props.onGovern("grants")}>
            projects
          </button>
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
        <span title="Baseline: every project's deploys obey it">
          <Icon icon="shield-check" size={12} />
          {isPermissive(env().protection) ? "no baseline rule" : rulesSummary(env().protection)}
        </span>
        <span title="Eligibility: which projects may deploy here">
          <Icon icon="user" size={12} />
          {eligibilityLabel(env().eligibility)}
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
 *  it is the key every pipeline's `environment:` names. */
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
    const n = name();
    if (props.taken.includes(n)) return `${n} is already registered — edit it instead.`;
    return nameError(n);
  };

  const save = async () => {
    const body = toPutBody({ description: description() }).body;
    if (nameProblem() || saving()) return;
    setSaving(true);
    setError(null);
    try {
      await putOrgEnvironment(props.org, creating() ? name() : props.env!.name, body);
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
                Exactly the name pipelines use — case matters. It cannot be changed
                later.
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
        </div>

        <div class="modal-foot">
          <Show when={nameProblem()}>
            <p class="error">{nameProblem()}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={!!nameProblem() || saving()} onClick={save}>
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

/** Shared dialog chrome: Escape closes, a click on the scrim closes. */
function useEscape(onClose: () => void) {
  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });
}

/** The baseline: the four rules every project's deploys here obey. A full
 *  replace (`toProtectionBody` sends every field). */
function BaselineDialog(props: {
  org: string;
  env: OrgEnvironment;
  onClose: () => void;
  onSaved: () => void;
}) {
  useEscape(props.onClose);
  const initial = rulesToForm(props.env.protection);
  const [approvers, setApprovers] = createSignal(initial.approvers);
  const [allowedRefs, setAllowedRefs] = createSignal(initial.allowedRefs);
  const [waitTimer, setWaitTimer] = createSignal(initial.waitTimerS);
  const [requireReason, setRequireReason] = createSignal(initial.requireReason);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const built = () =>
    toProtectionBody({
      approvers: approvers(),
      allowedRefs: allowedRefs(),
      waitTimerS: waitTimer(),
      requireReason: requireReason(),
    });

  const save = async () => {
    const body = built().body;
    if (!body || saving()) return;
    setSaving(true);
    setError(null);
    try {
      await putEnvironmentProtection(props.org, props.env.name, body);
      props.onSaved();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save the baseline.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal modal-tall" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h">
          <span>Baseline · {props.env.name}</span>
        </div>
        <div class="modal-body token-form">
          <div class="form-r">
            <label>approvers</label>
            <input
              class="input"
              placeholder="comma-separated user ids (blank = none)"
              value={approvers()}
              onInput={(e) => setApprovers(e.currentTarget.value)}
            />
            <PrincipalSearch
              org={props.org}
              placeholder="add an approver — find a user"
              onPick={(id) => setApprovers(addToList(approvers(), id))}
            />
            <p class="fieldnote">Every one must approve a deploy's gate, in every project.</p>
          </div>
          <div class="form-r">
            <label>allowed refs</label>
            <input
              class="input"
              placeholder="refs/heads/main, refs/tags/* (blank = any ref)"
              value={allowedRefs()}
              onInput={(e) => setAllowedRefs(e.currentTarget.value)}
            />
          </div>
          <div class="form-r">
            <label>wait timer (s)</label>
            <input
              class="input"
              type="number"
              min="0"
              value={waitTimer()}
              onInput={(e) => setWaitTimer(e.currentTarget.value)}
            />
            <p class="fieldnote">Recorded, not yet enforced.</p>
          </div>
          <div class="form-r">
            <label>require reason</label>
            <label class="checkbox-inline">
              <input
                type="checkbox"
                checked={requireReason()}
                onInput={(e) => setRequireReason(e.currentTarget.checked)}
              />
              <span class="subtle">manual/api dispatches must supply a reason</span>
            </label>
          </div>
        </div>
        <div class="modal-foot">
          <Show when={built().error}>
            <p class="error">{built().error}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={!!built().error || saving()} onClick={save}>
              {saving() ? "Saving…" : "Save baseline"}
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

/** Eligibility: all projects, or project-name patterns (`*` wildcards). */
function EligibilityDialog(props: {
  org: string;
  env: OrgEnvironment;
  onClose: () => void;
  onSaved: () => void;
}) {
  useEscape(props.onClose);
  const [all, setAll] = createSignal(props.env.eligibility.all_projects);
  const [patterns, setPatterns] = createSignal((props.env.eligibility.projects ?? []).join(", "));
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const save = async () => {
    const body = toEligibilityBody(all(), patterns()).body;
    if (!body || saving()) return;
    setSaving(true);
    setError(null);
    try {
      await putEnvironmentEligibility(props.org, props.env.name, body);
      props.onSaved();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save the eligibility.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h">
          <span>Who may deploy · {props.env.name}</span>
        </div>
        <div class="modal-body token-form">
          <div class="form-r">
            <label>all projects</label>
            <label class="checkbox-inline">
              <input type="checkbox" checked={all()} onInput={(e) => setAll(e.currentTarget.checked)} />
              <span class="subtle">every project in {props.org}</span>
            </label>
          </div>
          <Show when={!all()}>
            <div class="form-r">
              <label>projects</label>
              <input
                class="input mono"
                placeholder="payments, team-a-*"
                value={patterns()}
                onInput={(e) => setPatterns(e.currentTarget.value)}
              />
              <p class="fieldnote">
                Project (repository) names; <span class="mono">*</span> matches anything. Empty
                admits <b>no</b> project — a pipeline naming {props.env.name} is refused.
              </p>
            </div>
          </Show>
          <p class="fieldnote">
            Takes effect for the next run, the next gate approval and the next step that
            would receive this environment's secrets.
          </p>
        </div>
        <div class="modal-foot">
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={saving()} onClick={save}>
              {saving() ? "Saving…" : "Save"}
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

/** The projects with an overlay or a privilege grant here. Overlays are the
 *  projects' own (read-only here); grants are this admin's to give and
 *  withdraw — to one project at a time, never the Environment as a whole. */
function ProjectsDialog(props: { org: string; env: OrgEnvironment; onClose: () => void }) {
  useEscape(props.onClose);
  const [epoch, setEpoch] = createSignal(0);
  const [projects] = createResource(
    () => ({ org: props.org, env: props.env.name, epoch: epoch() }),
    (k) => listEnvironmentProjects(k.org, k.env),
  );
  const [project, setProject] = createSignal("");
  const [rows, setRows] = createSignal<ImageGrantRow[]>([
    { digest: "", privileged: false, capabilities: "" },
  ]);
  const [permitOverlay, setPermitOverlay] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const built = () => toGrantBody(rows(), permitOverlay());
  const problem = () => (project().trim() ? (built().error ?? null) : "Name a project.");
  const setRow = (i: number, patch: Partial<ImageGrantRow>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));

  /** Load a project's current grant into the form, to edit it. */
  const edit = (name: string) => {
    const grant = projects()?.find((p) => p.project === name)?.privilege_grant;
    setProject(name);
    setPermitOverlay(grant?.permit_k8s_overlay ?? false);
    setRows(
      grant && grant.privileged_images.length > 0
        ? grant.privileged_images.map((g) => ({
            digest: g.image_digest,
            privileged: g.privileged ?? false,
            capabilities: (g.capabilities ?? []).join(", "),
          }))
        : [{ digest: "", privileged: false, capabilities: "" }],
    );
  };

  const save = async () => {
    const body = built().body;
    if (!body || problem() || saving()) return;
    setSaving(true);
    setError(null);
    try {
      await putPrivilegeGrant(props.org, props.env.name, project().trim(), body);
      setEpoch((e) => e + 1);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save the grant.");
    } finally {
      setSaving(false);
    }
  };

  const withdraw = async (name: string) => {
    if (!window.confirm(`Withdraw ${name}'s privileges in ${props.env.name}?`)) return;
    setError(null);
    try {
      await deletePrivilegeGrant(props.org, props.env.name, name);
      setEpoch((e) => e + 1);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not withdraw the grant.");
    }
  };

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal modal-tall" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h">
          <span>Projects · {props.env.name}</span>
        </div>
        <div class="modal-body token-form">
          <Show when={!projects.loading} fallback={<p class="empty">loading…</p>}>
            <Show when={!projects.error} fallback={<p class="error">Could not load the projects.</p>}>
              <Show
                when={(projects() ?? []).length > 0}
                fallback={<p class="empty">No project has an overlay or privileges here.</p>}
              >
                <ul class="tok-list">
                  <For each={projects()}>
                    {(p) => (
                      <li class="tok">
                        <div class="tok-head">
                          <span class="tok-name mono">{p.project}</span>
                          <Show when={!p.eligible}>
                            <span class="tok-role">not eligible</span>
                          </Show>
                          <span class="oenv-actions">
                            <button class="btn btn-ghost btn-sm" onClick={() => edit(p.project)}>
                              {p.privilege_grant ? "edit grant" : "grant"}
                            </button>
                            <Show when={p.privilege_grant}>
                              <button class="btn btn-danger btn-sm" onClick={() => withdraw(p.project)}>
                                withdraw
                              </button>
                            </Show>
                          </span>
                        </div>
                        <div class="tok-facts">
                          <span title="The project's own tighten-only overlay">
                            <Icon icon="shield-check" size={12} />
                            {p.overlay ? rulesSummary(p.overlay) : "no overlay"}
                          </span>
                          <Show when={p.privilege_grant}>
                            {(g) => (
                              <span class="mono">
                                <Icon icon="key-round" size={12} />
                                {g().privileged_images.map((i) => i.image_digest.slice(0, 19)).join(", ") ||
                                  "no images"}
                                {g().permit_k8s_overlay ? " · raw k8s_overlay" : ""}
                              </span>
                            )}
                          </Show>
                        </div>
                      </li>
                    )}
                  </For>
                </ul>
              </Show>
            </Show>
          </Show>

          <div class="form-r">
            <label>grant privileges to</label>
            <input
              class="input mono"
              placeholder="project (repository) name"
              value={project()}
              onInput={(e) => setProject(e.currentTarget.value)}
            />
          </div>
          <div class="form-r">
            <label>image digests</label>
            <div class="oenv-rows">
              <For each={rows()}>
                {(row, i) => (
                  <div class="oenv-row">
                    <input
                      class="input mono"
                      placeholder="sha256:…"
                      value={row.digest}
                      onInput={(e) => setRow(i(), { digest: e.currentTarget.value })}
                    />
                    <input
                      class="input mono"
                      placeholder="capabilities (NET_ADMIN, …)"
                      value={row.capabilities}
                      onInput={(e) => setRow(i(), { capabilities: e.currentTarget.value })}
                    />
                    <label class="checkbox-inline" title="May run as a privileged container">
                      <input
                        type="checkbox"
                        checked={row.privileged}
                        onInput={(e) => setRow(i(), { privileged: e.currentTarget.checked })}
                      />
                      <span class="subtle">privileged</span>
                    </label>
                    <button
                      class="btn btn-ghost btn-sm"
                      title="Remove this image"
                      onClick={() => setRows((rs) => rs.filter((_, j) => j !== i()))}
                    >
                      ✕
                    </button>
                  </div>
                )}
              </For>
              <div>
                <button
                  class="btn btn-ghost btn-sm"
                  onClick={() => setRows((rs) => [...rs, { digest: "", privileged: false, capabilities: "" }])}
                >
                  <Icon icon="plus" size={12} /> add image
                </button>
              </div>
            </div>
            <p class="fieldnote">
              A step may use a governed grant only with an image pinned to one of these
              digests (<span class="mono">repo@sha256:…</span>). Saving replaces the project's
              whole grant.
            </p>
          </div>
          <div class="form-r">
            <label>raw k8s_overlay</label>
            <label class="checkbox-inline">
              <input
                type="checkbox"
                checked={permitOverlay()}
                onInput={(e) => setPermitOverlay(e.currentTarget.checked)}
              />
              <span class="subtle">this project's steps here may set a raw k8s_overlay</span>
            </label>
          </div>
        </div>
        <div class="modal-foot">
          <Show when={problem() && project().trim()}>
            <p class="error">{problem()}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={!!problem() || saving()} onClick={save}>
              {saving() ? "Saving…" : "Save grant"}
            </button>
            <button class="btn btn-ghost" onClick={props.onClose}>
              Close
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

/** The migration's split report (ADR-0071 §10 step 8): each name merged
 *  across projects, or opened to all projects, with each project's old rules
 *  beside its overlay now. Shown only to org admins (the read needs
 *  Administer) and only while it has lines. */
function MigrationReport(props: {
  org: string;
  lines: MigrationReportLine[];
  onChanged: () => void;
}) {
  const [open, setOpen] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const banner = () => reportBanner(props.lines);

  const acknowledge = async (env: string) => {
    setError(null);
    try {
      await acknowledgeMigrationReport(props.org, env);
      props.onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : `Could not acknowledge ${env}.`);
    }
  };

  return (
    <div class="oenv-report">
      <div class="oenv-report-h">
        <Icon icon="alert-triangle" size={14} />
        <span>{banner() ?? "Migration report — every line reviewed."}</span>
        <button class="btn btn-ghost btn-sm" onClick={() => setOpen((o) => !o)}>
          {open() ? "hide" : "review"}
        </button>
      </div>
      <Show when={open()}>
        <p class="fieldnote">
          Per-repository environments of one name became one org environment, and each
          project's old rules became its own overlay — so every project deploys exactly as
          before. If a name was really two places (one <span class="mono">prod</span> that is{" "}
          <span class="mono">prod-eu</span>), split it in YAML: change those projects'{" "}
          <span class="mono">environment:</span> and create the new environment here.
        </p>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
        <ul class="tok-list">
          <For each={props.lines}>
            {(line) => (
              <li class="tok">
                <div class="tok-head">
                  <span class="tok-name mono">{line.environment}</span>
                  <Show when={line.referenced_only}>
                    <span class="tok-role" title="No project defined it; it was opened to all projects">
                      all projects
                    </span>
                  </Show>
                  <Show when={line.environment_exists && line.permissive_baseline}>
                    <span class="tok-role">permissive baseline — set a baseline</span>
                  </Show>
                  <Show when={!line.environment_exists}>
                    <span class="tok-role">deleted since</span>
                  </Show>
                  <span class="oenv-actions">
                    <Show
                      when={line.acknowledged_at == null}
                      fallback={
                        <span class="subtle" title={absTime(line.acknowledged_at!)}>
                          reviewed by {line.acknowledged_by}
                        </span>
                      }
                    >
                      <button class="btn btn-ghost btn-sm" onClick={() => acknowledge(line.environment)}>
                        mark reviewed
                      </button>
                    </Show>
                  </span>
                </div>
                <div class="tok-facts">
                  <Show when={line.merged_projects.length > 0}>
                    <span>merged from {line.merged_projects.join(", ")}</span>
                  </Show>
                  <Show when={line.referencing_projects.length > 0}>
                    <span>named by {line.referencing_projects.join(", ")}</span>
                  </Show>
                  <Show when={line.differing_rules.length > 0}>
                    <span>
                      <Icon icon="alert-triangle" size={12} /> differed on{" "}
                      {line.differing_rules.join(", ")}
                    </span>
                  </Show>
                </div>
                <div class="oenv-report-projects">
                  <For each={line.projects}>
                    {(p) => (
                      <div class="env-h-row mono">
                        <b>{p.project}</b>{" "}
                        <span class="subtle">was</span>{" "}
                        {p.legacy
                          ? `${p.legacy.approvers.length} approver(s) · ${
                              p.legacy.allowed_refs.join(" | ") || "any ref"
                            }${p.legacy.require_reason ? " · reason" : ""}${
                              p.legacy.privileged_images.length || p.legacy.permit_k8s_overlay
                                ? " · privileges"
                                : ""
                            }`
                          : "no rules (only named it)"}{" "}
                        <span class="subtle">· now</span>{" "}
                        {p.overlay ? rulesSummary(p.overlay) : "no overlay"}
                      </div>
                    )}
                  </For>
                </div>
              </li>
            )}
          </For>
        </ul>
      </Show>
    </div>
  );
}
