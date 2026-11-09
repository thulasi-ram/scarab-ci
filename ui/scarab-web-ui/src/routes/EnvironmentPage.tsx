// One org Environment, on two pages (git-bug c11abc5).
//
// `/orgs/<org>/settings/environments/<name>` shows it, read-only: the
// description, the baseline every project's deploys obey, which projects may
// deploy, and each project's overlay and privilege grant. One Edit button goes
// to `…/<name>/edit`, which holds every change: description, baseline and
// eligibility (one Save, sending only the parts that changed), each project's
// privilege grant (saved per project), and Delete. Withdrawing a grant and
// deleting the Environment each ask for the exact name to be typed.
//
// The name cannot be changed: it is the key every pipeline's `environment:`
// and the OIDC subject (`…/env/<name>/…`) use. The display name can: headings
// show it, with the name beside it in mono. To move to another name, Duplicate
// (on the view) copies the Environment, its rules, eligibility, overlays,
// grants and secrets under the new name, and lists what it did not copy.
//
// Both render inside the settings frame, with Environments marked in the
// sidebar, and each has its own URL to send as a link. The frame shows them
// only to an admin of the org; the server enforces Administer on every
// request.
import { createMemo, createResource, createSignal, For, Show, type JSX } from "solid-js";
import { A, useNavigate, useParams } from "@solidjs/router";
import {
  deleteOrgEnvironment,
  deletePrivilegeGrant,
  duplicateOrgEnvironment,
  getOrgEnvironment,
  listEnvironmentProjects,
  listOrgEnvironments,
  putEnvironmentEligibility,
  putEnvironmentProtection,
  putOrgEnvironment,
  putPrivilegeGrant,
  type DuplicateEnvironmentResult,
  type EnvironmentDeleteBlockers,
  type OrgEnvironment,
} from "../api/client";
import {
  eligibilityLabel,
  isPermissive,
  rulesSummary,
  toGrantBody,
  unprotectedButOpen,
  type ImageGrantRow,
  type RulesForm,
} from "../environment-overlays";
import {
  approvalGateFacts,
  blockerLines,
  cascadeDeleteLabel,
  copiedSummary,
  displayNameError,
  durationText,
  formHasApprovers,
  idleTimerWarning,
  requiredApprovalsError,
  type ApprovalGateForm,
  duplicateChecklist,
  environmentEditHref,
  environmentHref,
  environmentLabel,
  environmentsHref,
  nameBeside,
  planSave,
  runSave,
  shownName,
  toDuplicateBody,
  toEditForm,
} from "../org-environments";
import { addToList } from "../identities";
import { decodeParam } from "../settings-nav";
import { SettingsCrumbs, SettingsFrame, type Crumb } from "./Settings";
import { absTime, relTime } from "../fmt";
import PrincipalSearch from "../components/PrincipalSearch";
import FormSection from "../components/FormSection";
import Icon from "../components/Icon";
import ConfirmDestructive from "../components/ConfirmDestructive";
import Modal, { ModalMessage } from "../components/Modal";
import { Checkbox, Field, Input, Select } from "../components/Field";
import { splitList } from "../environment-overlays";

/**
 * The settings frame around one Environment's page, with the org and name
 * from the route, decoded. The page is built again for each org and name:
 * what it loads and edits belongs to the ones it was built with.
 */
function EnvironmentFrame(props: { children: (org: string, name: string) => JSX.Element }) {
  const params = useParams<{ org: string; env: string }>();
  const org = () => decodeParam(params.org);
  const name = () => decodeParam(params.env);
  const key = createMemo(() => JSON.stringify([org(), name()]));
  return (
    <SettingsFrame kind="org" org={org()} section="environments">
      {/* The child keeps its parameter: Show calls a child function only
          when it takes one. */}
      <Show when={key()} keyed>
        {(_key) => props.children(org(), name())}
      </Show>
    </SettingsFrame>
  );
}

/** The Environment by name (`GET …/environments/{env}`). `undefined` while
 *  loading; `null` when there is none of that name. */
function useEnvironment(org: string, name: string) {
  const [env, { refetch }] = createResource(() => getOrgEnvironment(org, name));
  return [env, refetch] as const;
}

/** The trail both pages share, at the top of the settings main column. It
 *  shows the display name once the Environment has loaded, and the name
 *  pipelines use beside the org below it. */
function Head(props: { org: string; name: string; env?: OrgEnvironment | null; edit?: boolean }) {
  const record = () => ({ name: props.name, display_name: props.env?.display_name });
  const here = (): Crumb => ({
    label: shownName(record()),
    mono: nameBeside(record()) === null,
  });
  const trail = (): Crumb[] =>
    props.edit
      ? [
          { label: "Environments", href: environmentsHref(props.org) },
          { ...here(), href: environmentHref(props.org, props.name) },
          { label: "Edit" },
        ]
      : [{ label: "Environments", href: environmentsHref(props.org) }, here()];
  return (
    <SettingsCrumbs
      trail={trail()}
      sub={
        <>
          <span class="mono">{props.org}</span>
          <Show when={nameBeside(record())}>
            {(name) => (
              <>
                {" · "}
                <span class="mono" title="The name pipelines use">
                  {name()}
                </span>
              </>
            )}
          </Show>{" "}
          · {props.edit ? "edit an environment" : "where deploys land"}
        </>
      }
    />
  );
}

/** Load errors, loading and not-found, around `children`. Signing in and
 *  administering the org are the frame's to check. */
function Guard(props: {
  org: string;
  name: string;
  env: OrgEnvironment | null | undefined;
  loading: boolean;
  error: unknown;
  children: (env: OrgEnvironment) => JSX.Element;
}) {
  return (
    <Show when={!props.error} fallback={<p class="error">Could not load this environment.</p>}>
      <Show when={!props.loading || props.env} fallback={<p class="empty">loading…</p>}>
        {/* Keyed: another Environment (or a reload) renders afresh, so
            the edit form starts from what is stored. */}
        <Show
          when={props.env}
          keyed
          fallback={
            <p class="empty">
              There is no environment named <b class="mono">{props.name}</b> in {props.org}.{" "}
              <A href={environmentsHref(props.org)}>All environments</A>
            </p>
          }
        >
          {(env) => props.children(env)}
        </Show>
      </Show>
    </Show>
  );
}

// ── View ─────────────────────────────────────────────────────────────────────

/** `/orgs/:org/settings/environments/:env` — the Environment, read-only. */
export function EnvironmentView() {
  return <EnvironmentFrame>{(org, name) => <View org={org} name={name} />}</EnvironmentFrame>;
}

function View(props: { org: string; name: string }) {
  const [env] = useEnvironment(props.org, props.name);
  const [duplicating, setDuplicating] = createSignal(false);
  return (
    <>
      <Head org={props.org} name={props.name} env={env()} />
      <Guard org={props.org} name={props.name} env={env()} loading={env.loading} error={env.error}>
        {(e) => (
          <>
            <div class="run-toolbar">
              <A href={environmentEditHref(props.org, e.name)} class="btn btn-primary btn-sm">
                Edit
              </A>
              <button class="btn btn-ghost btn-sm" onClick={() => setDuplicating(true)}>
                <Icon icon="copy" size={14} /> Duplicate
              </button>
            </div>
            <Show when={duplicating()}>
              <DuplicateDialog org={props.org} env={e} onClose={() => setDuplicating(false)} />
            </Show>
            <Details env={e} />
            <ProjectsPanel org={props.org} env={e.name} />
          </>
        )}
      </Guard>
    </>
  );
}

function Details(props: { env: OrgEnvironment }) {
  const rules = () => props.env.protection;
  const list = (xs: readonly string[], none: string) => (xs.length > 0 ? xs.join(", ") : none);
  return (
    <>
    <Show when={unprotectedButOpen(props.env)}>
      <p class="tok-warn env-warn" role="status">
        <Icon icon="alert-triangle" size={14} /> Projects may deploy here and no baseline rule
        protects it: only each project's own overlay does. Edit it to set a baseline.
      </p>
    </Show>
    <Show when={idleTimerWarning(rules())}>
      {(w) => (
        <p class="tok-warn env-warn" role="status">
          <Icon icon="alert-triangle" size={14} /> {w()}
        </p>
      )}
    </Show>
    <div class="panel">
      <div class="panel-h">
        <span>Environment</span>
        <Show when={isPermissive(rules())}>
          <span class="tok-role" title="No org-wide rule protects this place yet, only each project's own overlay">
            permissive baseline
          </span>
        </Show>
      </div>
      <dl class="lib-facts">
        <dt>name</dt>
        <dd class="m">{props.env.name}</dd>
        <dt>display name</dt>
        <dd>{props.env.display_name || <span class="subtle">none: the name shows</span>}</dd>
        <dt>description</dt>
        <dd>{props.env.description || <span class="subtle">none</span>}</dd>
        <dt>approvers</dt>
        <dd class="m">{list(rules().approvers, "none")}</dd>
        <dt>allowed refs</dt>
        <dd class="m">{list(rules().allowed_refs, "any ref")}</dd>
        <dt>wait timer</dt>
        <dd>
          {rules().wait_timer_s > 0 ? durationText(rules().wait_timer_s) : "none"}
          <Show when={rules().approvers.length > 0 && rules().wait_timer_s > 0}>
            <span class="subtle"> after the approvals are in</span>
          </Show>
        </dd>
        <dt>reason</dt>
        <dd>{rules().require_reason ? "manual and API dispatches must give one" : "not required"}</dd>
        <For each={approvalGateFacts(rules().approval_gate, rules().approvers.length)}>
          {(f) => (
            <>
              <dt>{f.label}</dt>
              <dd>{f.value}</dd>
            </>
          )}
        </For>
        <dt>projects that may use {shownName(props.env)}</dt>
        <dd class="m">{eligibilityLabel(props.env.eligibility)}</dd>
        <dt>last changed</dt>
        <dd>
          <span class="mono">{props.env.updated_by}</span> ·{" "}
          <span title={absTime(props.env.updated_at)}>{relTime(props.env.updated_at)}</span>
        </dd>
      </dl>
    </div>
    </>
  );
}

/** The projects with an overlay or a privilege grant here, read-only. On the
 *  edit page each row also carries grant and withdraw. */
function ProjectsPanel(props: {
  org: string;
  env: string;
  epoch?: number;
  actions?: (p: { project: string; hasGrant: boolean }) => JSX.Element;
}) {
  const [projects] = createResource(
    () => ({ org: props.org, env: props.env, epoch: props.epoch ?? 0 }),
    (k) => listEnvironmentProjects(k.org, k.env),
  );
  return (
    <div class="panel">
      <div class="panel-h">
        <span>Project overlays and grants</span>
      </div>
      <div class="secrets-body">
        <Show when={!projects.loading || projects()} fallback={<p class="empty">loading…</p>}>
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
                        <Show when={props.actions}>
                          {(a) => (
                            <span class="oenv-actions">
                              {a()({ project: p.project, hasGrant: !!p.privilege_grant })}
                            </span>
                          )}
                        </Show>
                      </div>
                      <div class="tok-facts">
                        <span title="The project's own overlay, which can only tighten the baseline">
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
      </div>
    </div>
  );
}

// ── Edit ─────────────────────────────────────────────────────────────────────

/** `/orgs/:org/settings/environments/:env/edit` — every change to it. */
export function EnvironmentEdit() {
  return <EnvironmentFrame>{(org, name) => <Edit org={org} name={name} />}</EnvironmentFrame>;
}

function Edit(props: { org: string; name: string }) {
  const [env] = useEnvironment(props.org, props.name);
  return (
    <>
      <Head org={props.org} name={props.name} env={env()} edit />
      <Guard org={props.org} name={props.name} env={env()} loading={env.loading} error={env.error}>
        {(e) => <EditForm org={props.org} env={e} />}
      </Guard>
    </>
  );
}

function EditForm(props: { org: string; env: OrgEnvironment }) {
  const nav = useNavigate();
  const initial = toEditForm(props.env);
  const [displayName, setDisplayName] = createSignal(initial.displayName);
  const [description, setDescription] = createSignal(initial.description);
  const [baseline, setBaseline] = createSignal<RulesForm>(initial.baseline);
  const [gate, setGate] = createSignal<ApprovalGateForm>(initial.gate);
  const [allProjects, setAllProjects] = createSignal(initial.allProjects);
  const [patterns, setPatterns] = createSignal(initial.patterns);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [grantsEpoch, setGrantsEpoch] = createSignal(0);
  const setRule = (patch: Partial<RulesForm>) => setBaseline((b) => ({ ...b, ...patch }));
  const setGateField = (patch: Partial<ApprovalGateForm>) => setGate((g) => ({ ...g, ...patch }));
  const hasApprovers = () => formHasApprovers(baseline());
  const approverCount = () => splitList(baseline().approvers).length;

  const plan = () =>
    planSave(props.env, {
      displayName: displayName(),
      description: description(),
      baseline: baseline(),
      gate: gate(),
      allProjects: allProjects(),
      patterns: patterns(),
    });
  const viewHref = () => environmentHref(props.org, props.env.name);

  const save = async () => {
    const writes = plan().writes;
    if (!writes || writes.length === 0 || saving()) return;
    setSaving(true);
    setError(null);
    try {
      const problem = await runSave(writes, {
        // Re-read first: the details write would recreate an
        // Environment another admin deleted since this page loaded.
        stillThere: async () => (await getOrgEnvironment(props.org, props.env.name)) !== null,
        send: (w) =>
          w.part === "details"
            ? putOrgEnvironment(props.org, props.env.name, w.body)
            : w.part === "baseline"
              ? putEnvironmentProtection(props.org, props.env.name, w.body)
              : putEnvironmentEligibility(props.org, props.env.name, w.body),
      });
      if (problem !== null) {
        setError(problem);
        return;
      }
      nav(viewHref());
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="env-edit">
      <div class="panel">
        <div class="panel-h">
          <span>Environment</span>
        </div>
        <div class="secrets-body token-form">
          <FormSection n={1} title="Name">
            <Input mono readOnly aria-label="name" value={props.env.name} onValue={() => {}} />
            <p class="f-help">
              The name pipelines put in <span class="mono">environment:</span>. It cannot be
              changed: to use another name, duplicate this environment and change the pipelines.
            </p>
          </FormSection>

          <FormSection n={2} title="Display name">
            <Field
              id="env-display-name"
              label="Display name"
              help="What lists and headings show, with the name beside it. Blank shows the name."
              error={displayNameError(displayName())}
            >
              <Input placeholder={props.env.name} value={displayName()} onValue={setDisplayName} />
            </Field>
          </FormSection>

          <FormSection n={3} title="Description">
            <Input
              id="env-description"
              aria-label="description"
              placeholder="what lands here, who runs it"
              value={description()}
              onValue={setDescription}
            />
          </FormSection>

          <FormSection n={4} title="Baseline" note="Every project's deploys here obey these rules. A project can add to them for itself, never loosen them.">
            <Field id="env-approvers" label="Approvers" help="Who approves this environment's approval gate, in every project. User ids, comma-separated.">
              <Input
                mono
                placeholder="none"
                value={baseline().approvers}
                onValue={(v) => setRule({ approvers: v })}
              />
            </Field>
            <PrincipalSearch
              org={props.org}
              placeholder="Add an approver: find a user"
              onPick={(id) => setRule({ approvers: addToList(baseline().approvers, id) })}
            />
            <Field
              id="env-refs"
              label="Allowed refs"
              help={
                <>
                  Full refs, comma-separated, e.g. <span class="mono">refs/heads/main</span>,{" "}
                  <span class="mono">refs/tags/*</span>.
                </>
              }
            >
              <Input
                mono
                placeholder="blank allows any ref"
                value={baseline().allowedRefs}
                onValue={(v) => setRule({ allowedRefs: v })}
              />
            </Field>
            <Field
              id="env-wait"
              label="Wait timer"
              help={
                hasApprovers()
                  ? "A minimum wait on the approval gate, counted from when the gate is reached. Approvals given early count when it runs out."
                  : "Needs approvers: the timer waits on the approval gate, which only an environment with approvers has."
              }
            >
              <Input
                mono
                inputmode="numeric"
                suffix="seconds"
                disabled={!hasApprovers()}
                value={hasApprovers() ? baseline().waitTimerS : "0"}
                onValue={(v) => setRule({ waitTimerS: v })}
              />
            </Field>
            <Show when={idleTimerWarning(props.env.protection)}>
              {(w) => (
                <p class="tok-warn env-warn" role="status">
                  <Icon icon="alert-triangle" size={14} /> {w()} Saving the baseline without approvers
                  sets it to 0.
                </p>
              )}
            </Show>
            <Checkbox
              checked={baseline().requireReason}
              onChecked={(v) => setRule({ requireReason: v })}
              label="Require a reason"
              help="Manual and API dispatches must give one."
            />
          </FormSection>

          <FormSection
            n={5}
            title="Approval gate"
            note="A manual gate with a step after it that uses this environment's authority: its secrets, a privilege grant, a k8s_overlay, a write forge token or the deploy mark. Only these gates need the approvers. They apply once the environment has approvers."
          >
            <Field
              id="env-missing-gate"
              label="When a pipeline has no approval gate"
              help={
                gate().missingGateMode === "add"
                  ? "Scarab adds one manual gate to the run, in front of the first steps that use this environment."
                  : "The run is refused, naming the steps and the YAML to add."
              }
            >
              <Select
                value={gate().missingGateMode}
                onValue={(v) => setGateField({ missingGateMode: v === "refuse" ? "refuse" : "add" })}
              >
                <option value="add">Add an approval gate</option>
                <option value="refuse">Refuse the run</option>
              </Select>
            </Field>
            <Field
              id="env-added-expiry"
              label="Added gate expiry"
              help={
                gate().addedGateExpiryS.trim()
                  ? "The gate Scarab adds fails if no one approves it in this time. A gate the pipeline wrote keeps its own expiry."
                  : "Blank: the added gate waits until someone approves or rejects it."
              }
            >
              <Input
                mono
                inputmode="numeric"
                suffix="seconds"
                placeholder="no expiry"
                value={gate().addedGateExpiryS}
                onValue={(v) => setGateField({ addedGateExpiryS: v })}
              />
            </Field>
            <Field
              id="env-required-approvals"
              label="Required approvals"
              error={requiredApprovalsError(gate().requiredApprovals, approverCount())}
              help={
                hasApprovers()
                  ? `How many of the ${approverCount()} approvers must approve. Blank: every one. A project's own approvers are each required as well.`
                  : "Needs approvers."
              }
            >
              <Input
                mono
                inputmode="numeric"
                placeholder={hasApprovers() ? `every approver (${approverCount()})` : "no approvers"}
                disabled={!hasApprovers()}
                value={gate().requiredApprovals}
                onValue={(v) => setGateField({ requiredApprovals: v })}
              />
            </Field>
            <Field
              id="env-max-age"
              label="Maximum approval age"
              help="A step that uses this environment is refused at launch once its approval gate released longer ago than this. Blank: 24 hours (86400)."
            >
              <Input
                mono
                inputmode="numeric"
                suffix="seconds"
                placeholder="86400"
                value={gate().maxApprovalAgeS}
                onValue={(v) => setGateField({ maxApprovalAgeS: v })}
              />
            </Field>
          </FormSection>

          <FormSection n={6} title={`Projects that may use ${shownName(props.env)}`}>
            <Checkbox
              checked={allProjects()}
              onChecked={setAllProjects}
              label={`Every project in ${props.org}`}
            />
            <Show when={!allProjects()}>
              <Field
                id="env-projects"
                label="Projects"
                help={
                  <>
                    Project (repository) names, case ignored; <span class="mono">*</span> matches
                    anything. Empty admits no project: a pipeline naming {props.env.name} is
                    refused.
                  </>
                }
              >
                <Input mono placeholder="payments, team-a-*" value={patterns()} onValue={setPatterns} />
              </Field>
            </Show>
            <p class="f-help">
              Takes effect for the next run, the next gate approval and the next step that would
              receive this environment's secrets.
            </p>
          </FormSection>
        </div>
        <div class="modal-foot">
          <Show when={plan().error}>
            <p class="error">{plan().error}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button
              class="btn btn-primary"
              disabled={!!plan().error || (plan().writes?.length ?? 0) === 0 || saving()}
              onClick={save}
            >
              {saving() ? "Saving…" : "Save"}
            </button>
            <A href={viewHref()} class="btn btn-ghost">
              Cancel
            </A>
          </div>
        </div>
      </div>

      <GrantsSection
        org={props.org}
        env={props.env.name}
        epoch={grantsEpoch()}
        onChanged={() => setGrantsEpoch((e) => e + 1)}
      />

      <DeleteSection org={props.org} name={props.env.name} />
    </div>
  );
}

/** A button that asks for `name` typed exactly, in a dialog, before `run`. */
function TypedConfirm(props: {
  name: string;
  /** What the opening button says. */
  label: string;
  /** The dialog's question. */
  title: string;
  /** One line: what happens as a result. */
  purpose: string;
  /** What the confirming button says. */
  confirm: string;
  run: () => Promise<void>;
}) {
  const [open, setOpen] = createSignal(false);
  return (
    <>
      <button class="btn btn-danger btn-sm" onClick={() => setOpen(true)}>
        {props.label}
      </button>
      <Show when={open()}>
        <ConfirmDestructive
          title={props.title}
          purpose={props.purpose}
          name={props.name}
          confirmLabel={props.confirm}
          onConfirm={props.run}
          onClose={() => setOpen(false)}
        />
      </Show>
    </>
  );
}

/** Per-project privilege grants: image digests and raw k8s_overlay, given to
 *  one project at a time, never to the Environment as a whole. Each grant
 *  saves on its own; withdrawing asks for the project's name. */
function GrantsSection(props: { org: string; env: string; epoch: number; onChanged: () => void }) {
  const blank = (): ImageGrantRow => ({ digest: "", privileged: false, capabilities: "" });
  const [project, setProject] = createSignal("");
  const [rows, setRows] = createSignal<ImageGrantRow[]>([blank()]);
  const [permitOverlay, setPermitOverlay] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const built = () => toGrantBody(rows(), permitOverlay());
  const problem = () => (project().trim() ? (built().error ?? null) : "Name a project.");
  const setRow = (i: number, patch: Partial<ImageGrantRow>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));

  /** Load a project's current grant into the form, to change it. */
  const load = async (name: string) => {
    setError(null);
    try {
      const grant = (await listEnvironmentProjects(props.org, props.env)).find(
        (p) => p.project === name,
      )?.privilege_grant;
      setProject(name);
      setPermitOverlay(grant?.permit_k8s_overlay ?? false);
      setRows(
        grant && grant.privileged_images.length > 0
          ? grant.privileged_images.map((g) => ({
              digest: g.image_digest,
              privileged: g.privileged ?? false,
              capabilities: (g.capabilities ?? []).join(", "),
            }))
          : [blank()],
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : `Could not load ${name}'s grant.`);
    }
  };

  const save = async () => {
    const body = built().body;
    if (!body || problem() || saving()) return;
    setSaving(true);
    setError(null);
    try {
      await putPrivilegeGrant(props.org, props.env, project().trim(), body);
      props.onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save the grant.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <>
      <ProjectsPanel
        org={props.org}
        env={props.env}
        epoch={props.epoch}
        actions={(p) => (
          <>
            <button class="btn btn-ghost btn-sm" onClick={() => load(p.project)}>
              {p.hasGrant ? "edit grant" : "grant"}
            </button>
            <Show when={p.hasGrant}>
              <TypedConfirm
                name={p.project}
                label="withdraw"
                title={`Withdraw ${p.project}'s privilege grant on ${props.env}?`}
                purpose={`${p.project}'s steps here can no longer use the privileges it gave.`}
                confirm="Withdraw grant"
                run={async () => {
                  await deletePrivilegeGrant(props.org, props.env, p.project);
                  props.onChanged();
                }}
              />
            </Show>
          </>
        )}
      />

      <div class="panel">
        <div class="panel-h">
          <span>Grant privileges</span>
        </div>
        <div class="secrets-body token-form">
          <p class="f-help">
            Saved per project, apart from the form above. Saving replaces the project's whole
            grant.
          </p>
          <Field id="env-grant-project" label="Project">
            <Input mono placeholder="project (repository) name" value={project()} onValue={setProject} />
          </Field>
          <div class="f">
            <div class="f-l">
              <span class="f-lbl">Image digests</span>
            </div>
            <div class="oenv-rows">
              <For each={rows()}>
                {(row, i) => (
                  <div class="oenv-row">
                    <Input
                      mono
                      aria-label="image digest"
                      placeholder="sha256:…"
                      value={row.digest}
                      onValue={(v) => setRow(i(), { digest: v })}
                    />
                    <Input
                      mono
                      aria-label="capabilities"
                      placeholder="capabilities (NET_ADMIN, …)"
                      value={row.capabilities}
                      onValue={(v) => setRow(i(), { capabilities: v })}
                    />
                    <span title="May run as a privileged container">
                      <Checkbox
                        checked={row.privileged}
                        onChecked={(v) => setRow(i(), { privileged: v })}
                        label="privileged"
                      />
                    </span>
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
                <button class="btn btn-ghost btn-sm" onClick={() => setRows((rs) => [...rs, blank()])}>
                  <Icon icon="plus" size={12} /> add image
                </button>
              </div>
            </div>
            <p class="f-help">
              A step may use a granted privilege only with an image pinned to one of these digests
              (<span class="mono">repo@sha256:…</span>).
            </p>
          </div>
          <Checkbox
            checked={permitOverlay()}
            onChecked={setPermitOverlay}
            label="This project's steps here may set a raw k8s_overlay"
          />
          <Show when={problem() && project().trim()}>
            <p class="error">{problem()}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary btn-sm" disabled={!!problem() || saving()} onClick={save}>
              {saving() ? "Saving…" : "Save grant"}
            </button>
          </div>
        </div>
      </div>
    </>
  );
}

/** Delete the Environment, after its name is typed. A plain delete is refused
 *  while projects hold overlays, grants or secrets in it; the dialog then
 *  lists them and its button deletes them with the Environment. */
function DeleteSection(props: { org: string; name: string }) {
  const nav = useNavigate();
  const [open, setOpen] = createSignal(false);
  const [blockers, setBlockers] = createSignal<EnvironmentDeleteBlockers | null>(null);
  const close = () => {
    setOpen(false);
    setBlockers(null);
  };
  const purpose = () =>
    `Pipelines naming ${props.name} are refused until an environment of that name exists again.`;
  const run = async () => {
    const known = blockers();
    const refused = await deleteOrgEnvironment(props.org, props.name, known !== null);
    if (refused !== null) {
      setBlockers(refused);
      // Keeps the dialog open: the button now names what it deletes too.
      throw new Error(
        "Projects still hold overlays, grants or secrets here. Delete them with the environment, or remove them first.",
      );
    }
    nav(environmentsHref(props.org));
  };
  return (
    <div class="panel env-danger">
      <div class="panel-h">
        <span>Delete</span>
      </div>
      <div class="secrets-body">
        <p>{purpose()}</p>
        <div>
          <button class="btn btn-danger btn-sm" onClick={() => setOpen(true)}>
            Delete environment
          </button>
          <Show when={open()}>
            <ConfirmDestructive
              title={`Delete ${props.name}?`}
              purpose={purpose()}
              name={props.name}
              confirmLabel={blockers() ? cascadeDeleteLabel(blockers()!) : "Delete environment"}
              onConfirm={run}
              onClose={close}
            >
              {blockers() && (
                <>
                  <p>These go with it, and cannot be brought back:</p>
                  <ul>
                    <For each={blockerLines(blockers()!)}>{(line) => <li>{line}</li>}</For>
                  </ul>
                </>
              )}
            </ConfirmDestructive>
          </Show>
        </div>
      </div>
    </div>
  );
}

// ── Duplicate ────────────────────────────────────────────────────────────────

/** Copy the Environment under a new name, then show what was copied and the
 *  checklist of what was not. Asks for a step-up: it copies secrets and the
 *  projects that may use it. */
function DuplicateDialog(props: { org: string; env: OrgEnvironment; onClose: () => void }) {
  const nav = useNavigate();
  const [taken] = createResource(
    () => props.org,
    (org) => listOrgEnvironments(org).then((es) => (es ?? []).map((e) => e.name)).catch(() => []),
  );
  const [name, setName] = createSignal("");
  const [displayName, setDisplayName] = createSignal("");
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [result, setResult] = createSignal<DuplicateEnvironmentResult | null>(null);
  const built = () => toDuplicateBody(name(), displayName(), taken() ?? []);
  const defaultShown = () => `${shownName(props.env)} (copy)`;

  const save = async () => {
    const body = built().body;
    if (!body || saving()) return;
    setSaving(true);
    setError(null);
    try {
      setResult(await duplicateOrgEnvironment(props.org, props.env.name, body));
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not duplicate the environment.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <Show
      when={result()}
      fallback={
        <Modal
          eyebrow={`${props.org} · environments`}
          title={`Duplicate ${environmentLabel(props.env)}`}
          purpose="A new environment with this one's baseline, eligibility, project overlays, privilege grants and secrets. This one is not changed."
          busy={saving()}
          onClose={props.onClose}
          after={{
            label: "after duplicating",
            value: "Pipelines still deploy into the old name until their environment: is changed.",
          }}
          footer={
            <>
              <Show when={error()}>
                <ModalMessage>{error()}</ModalMessage>
              </Show>
              <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={saving()}>
                Cancel
              </button>
              <button class="btn btn-primary" type="button" disabled={!!built().error || saving()} onClick={save}>
                {saving() ? "Duplicating…" : "Duplicate"}
              </button>
            </>
          }
        >
          <Field
            id="dup-env-name"
            label="New name"
            required
            help="Exactly the name pipelines will use; case matters. It cannot be changed later."
            error={name() ? (built().error ?? null) : null}
          >
            <Input mono autofocus placeholder="prod-eu" value={name()} onValue={setName} />
          </Field>
          <Field
            id="dup-env-display-name"
            label="Display name"
            help={`Blank shows "${defaultShown()}".`}
            error={displayNameError(displayName())}
          >
            <Input placeholder={defaultShown()} value={displayName()} onValue={setDisplayName} />
          </Field>
        </Modal>
      }
    >
      {(r) => <DuplicateResult org={props.org} old={props.env.name} result={r()} onClose={props.onClose} open={(n) => nav(environmentHref(props.org, n))} />}
    </Show>
  );
}

/** What a duplicate copied, and the checklist of what it did not. */
function DuplicateResult(props: {
  org: string;
  old: string;
  result: DuplicateEnvironmentResult;
  onClose: () => void;
  open: (name: string) => void;
}) {
  const created = () => props.result.environment;
  return (
    <Modal
      size="wide"
      eyebrow={`${props.org} · environments`}
      title={`Created ${environmentLabel(created())}`}
      purpose={`A copy of ${props.old}. ${props.old} is not changed.`}
      onClose={props.onClose}
      footer={
        <>
          <button class="btn btn-ghost" type="button" onClick={props.onClose}>
            Close
          </button>
          <button class="btn btn-primary" type="button" onClick={() => props.open(created().name)}>
            Open {shownName(created())}
          </button>
        </>
      }
    >
      <div class="dup-result">
        <h3 class="dup-h">Copied</h3>
        <ul class="dup-list">
          <For each={copiedSummary(props.result)}>{(line) => <li>{line}</li>}</For>
        </ul>
        <h3 class="dup-h">Not copied: to do</h3>
        <ol class="dup-list dup-todo">
          <For each={duplicateChecklist(props.old, props.result)}>
            {(item) => (
              <li>
                <b>{item.title}.</b> {item.detail}
                <Show when={item.items.length > 0}>
                  <div class="dup-items mono">{item.items.join(", ")}</div>
                </Show>
              </li>
            )}
          </For>
        </ol>
      </div>
    </Modal>
  );
}
