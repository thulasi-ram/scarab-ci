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
// uses, and the server has no rename.
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
  getOrgEnvironment,
  listEnvironmentProjects,
  putEnvironmentEligibility,
  putEnvironmentProtection,
  putOrgEnvironment,
  putPrivilegeGrant,
  type OrgEnvironment,
} from "../api/client";
import {
  eligibilityLabel,
  isPermissive,
  rulesSummary,
  toGrantBody,
  type ImageGrantRow,
  type RulesForm,
} from "../environment-overlays";
import {
  confirmsName,
  environmentEditHref,
  environmentHref,
  environmentsHref,
  planSave,
  toEditForm,
  type EnvironmentWrite,
} from "../org-environments";
import { addToList } from "../identities";
import { decodeParam } from "../settings-nav";
import { SettingsCrumbs, SettingsFrame, type Crumb } from "./Settings";
import { absTime, relTime } from "../fmt";
import PrincipalSearch from "../components/PrincipalSearch";
import FormSection from "../components/FormSection";
import Icon from "../components/Icon";

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

/** The trail both pages share, at the top of the settings main column. */
function Head(props: { org: string; name: string; edit?: boolean }) {
  const trail = (): Crumb[] =>
    props.edit
      ? [
          { label: "Environments", href: environmentsHref(props.org) },
          { label: props.name, href: environmentHref(props.org, props.name), mono: true },
          { label: "Edit" },
        ]
      : [{ label: "Environments", href: environmentsHref(props.org) }, { label: props.name, mono: true }];
  return (
    <SettingsCrumbs
      trail={trail()}
      sub={
        <>
          <span class="mono">{props.org}</span> · {props.edit ? "edit an environment" : "where deploys land"}
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
  return (
    <>
      <Head org={props.org} name={props.name} />
      <Guard org={props.org} name={props.name} env={env()} loading={env.loading} error={env.error}>
        {(e) => (
          <>
            <div class="run-toolbar">
              <A href={environmentEditHref(props.org, e.name)} class="btn btn-primary btn-sm">
                Edit
              </A>
            </div>
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
        <dt>description</dt>
        <dd>{props.env.description || <span class="subtle">none</span>}</dd>
        <dt>approvers</dt>
        <dd class="m">{list(rules().approvers, "none")}</dd>
        <dt>allowed refs</dt>
        <dd class="m">{list(rules().allowed_refs, "any ref")}</dd>
        <dt>wait timer</dt>
        <dd class="m">{rules().wait_timer_s}s</dd>
        <dt>reason</dt>
        <dd>{rules().require_reason ? "manual and API dispatches must give one" : "not required"}</dd>
        <dt>who may deploy</dt>
        <dd class="m">{eligibilityLabel(props.env.eligibility)}</dd>
        <dt>last changed</dt>
        <dd>
          <span class="mono">{props.env.updated_by}</span> ·{" "}
          <span title={absTime(props.env.updated_at)}>{relTime(props.env.updated_at)}</span>
        </dd>
      </dl>
    </div>
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
        <span>Projects</span>
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
      <Head org={props.org} name={props.name} edit />
      <Guard org={props.org} name={props.name} env={env()} loading={env.loading} error={env.error}>
        {(e) => <EditForm org={props.org} env={e} />}
      </Guard>
    </>
  );
}

const PART_LABEL: Record<EnvironmentWrite["part"], string> = {
  description: "description",
  baseline: "baseline",
  eligibility: "list of projects that may deploy",
};

function EditForm(props: { org: string; env: OrgEnvironment }) {
  const nav = useNavigate();
  const initial = toEditForm(props.env);
  const [description, setDescription] = createSignal(initial.description);
  const [baseline, setBaseline] = createSignal<RulesForm>(initial.baseline);
  const [allProjects, setAllProjects] = createSignal(initial.allProjects);
  const [patterns, setPatterns] = createSignal(initial.patterns);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [grantsEpoch, setGrantsEpoch] = createSignal(0);
  const setRule = (patch: Partial<RulesForm>) => setBaseline((b) => ({ ...b, ...patch }));

  const plan = () =>
    planSave(props.env, {
      description: description(),
      baseline: baseline(),
      allProjects: allProjects(),
      patterns: patterns(),
    });
  const viewHref = () => environmentHref(props.org, props.env.name);

  const save = async () => {
    const writes = plan().writes;
    if (!writes || writes.length === 0 || saving()) return;
    setSaving(true);
    setError(null);
    const done: string[] = [];
    try {
      for (const w of writes) {
        try {
          if (w.part === "description") await putOrgEnvironment(props.org, props.env.name, w.body);
          else if (w.part === "baseline") await putEnvironmentProtection(props.org, props.env.name, w.body);
          else await putEnvironmentEligibility(props.org, props.env.name, w.body);
          done.push(PART_LABEL[w.part]);
        } catch (e) {
          const why = e instanceof Error ? e.message : "the server refused it";
          const saved = done.length > 0 ? ` The ${done.join(" and ")} saved.` : "";
          setError(`Could not save the ${PART_LABEL[w.part]}: ${why}.${saved}`);
          return;
        }
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
            <div class="input input-static mono">{props.env.name}</div>
            <p class="fieldnote">
              The name pipelines put in <span class="mono">environment:</span>. It cannot be
              changed: to use another name, create that environment and change the pipelines.
            </p>
          </FormSection>

          <FormSection n={2} title="Description">
            <input
              id="env-description"
              class="input"
              aria-label="description"
              placeholder="what lands here, who runs it"
              value={description()}
              onInput={(e) => setDescription(e.currentTarget.value)}
            />
          </FormSection>

          <FormSection n={3} title="Baseline" note="Every project's deploys here obey these rules. A project can add to them for itself, never loosen them.">
            <div class="form-r">
              <label for="env-approvers">approvers</label>
              <input
                id="env-approvers"
                class="input mono"
                placeholder="comma-separated user ids (blank = none)"
                value={baseline().approvers}
                onInput={(e) => setRule({ approvers: e.currentTarget.value })}
              />
              <PrincipalSearch
                org={props.org}
                placeholder="add an approver: find a user"
                onPick={(id) => setRule({ approvers: addToList(baseline().approvers, id) })}
              />
              <p class="fieldnote">Every one must approve a deploy's gate, in every project.</p>
            </div>
            <div class="form-r">
              <label for="env-refs">allowed refs</label>
              <input
                id="env-refs"
                class="input mono"
                placeholder="refs/heads/main, refs/tags/* (blank = any ref)"
                value={baseline().allowedRefs}
                onInput={(e) => setRule({ allowedRefs: e.currentTarget.value })}
              />
            </div>
            <div class="form-r">
              <label for="env-wait">wait timer (s)</label>
              <input
                id="env-wait"
                class="input mono"
                type="number"
                min="0"
                value={baseline().waitTimerS}
                onInput={(e) => setRule({ waitTimerS: e.currentTarget.value })}
              />
              <p class="fieldnote">Recorded, not yet enforced.</p>
            </div>
            <div class="form-r">
              <label class="checkbox-inline">
                <input
                  type="checkbox"
                  checked={baseline().requireReason}
                  onInput={(e) => setRule({ requireReason: e.currentTarget.checked })}
                />
                <span>manual and API dispatches must give a reason</span>
              </label>
            </div>
          </FormSection>

          <FormSection n={4} title="Who may deploy">
            <div class="form-r">
              <label class="checkbox-inline">
                <input
                  type="checkbox"
                  checked={allProjects()}
                  onInput={(e) => setAllProjects(e.currentTarget.checked)}
                />
                <span>every project in {props.org}</span>
              </label>
            </div>
            <Show when={!allProjects()}>
              <div class="form-r">
                <label for="env-projects">projects</label>
                <input
                  id="env-projects"
                  class="input mono"
                  placeholder="payments, team-a-*"
                  value={patterns()}
                  onInput={(e) => setPatterns(e.currentTarget.value)}
                />
                <p class="fieldnote">
                  Project (repository) names; <span class="mono">*</span> matches anything. Empty
                  admits no project: a pipeline naming {props.env.name} is refused.
                </p>
              </div>
            </Show>
            <p class="fieldnote">
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

/** Ask for `name` typed exactly before `run`; a button until opened. */
function TypedConfirm(props: {
  name: string;
  /** What the opening button says. */
  label: string;
  /** What the confirming button says. */
  confirm: string;
  run: () => Promise<void>;
}) {
  const [open, setOpen] = createSignal(false);
  const [typed, setTyped] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const go = async () => {
    if (!confirmsName(typed(), props.name) || busy()) return;
    setBusy(true);
    setError(null);
    try {
      await props.run();
      setOpen(false);
      setTyped("");
    } catch (e) {
      setError(e instanceof Error ? e.message : "The server refused it.");
    } finally {
      setBusy(false);
    }
  };
  return (
    <Show
      when={open()}
      fallback={
        <button class="btn btn-danger btn-sm" onClick={() => setOpen(true)}>
          {props.label}
        </button>
      }
    >
      <div class="typed-confirm">
        <label>
          Type <b class="mono">{props.name}</b> to confirm
          <input
            class="input mono"
            aria-label={`Type ${props.name} to confirm`}
            value={typed()}
            onInput={(e) => setTyped(e.currentTarget.value)}
            autofocus
          />
        </label>
        <div class="modal-actions">
          <button
            class="btn btn-danger btn-sm"
            disabled={!confirmsName(typed(), props.name) || busy()}
            onClick={go}
          >
            {props.confirm}
          </button>
          <button
            class="btn btn-ghost btn-sm"
            onClick={() => {
              setOpen(false);
              setTyped("");
              setError(null);
            }}
          >
            Cancel
          </button>
        </div>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
      </div>
    </Show>
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
          <p class="fieldnote">
            Saved per project, apart from the form above. Saving replaces the project's whole
            grant.
          </p>
          <div class="form-r">
            <label for="env-grant-project">project</label>
            <input
              id="env-grant-project"
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
                      aria-label="image digest"
                      placeholder="sha256:…"
                      value={row.digest}
                      onInput={(e) => setRow(i(), { digest: e.currentTarget.value })}
                    />
                    <input
                      class="input mono"
                      aria-label="capabilities"
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
                <button class="btn btn-ghost btn-sm" onClick={() => setRows((rs) => [...rs, blank()])}>
                  <Icon icon="plus" size={12} /> add image
                </button>
              </div>
            </div>
            <p class="fieldnote">
              A step may use a granted privilege only with an image pinned to one of these digests
              (<span class="mono">repo@sha256:…</span>).
            </p>
          </div>
          <div class="form-r">
            <label class="checkbox-inline">
              <input
                type="checkbox"
                checked={permitOverlay()}
                onInput={(e) => setPermitOverlay(e.currentTarget.checked)}
              />
              <span>this project's steps here may set a raw k8s_overlay</span>
            </label>
          </div>
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

/** Delete the Environment, after its name is typed. */
function DeleteSection(props: { org: string; name: string }) {
  const nav = useNavigate();
  return (
    <div class="panel env-danger">
      <div class="panel-h">
        <span>Delete</span>
      </div>
      <div class="secrets-body">
        <p>
          Pipelines naming <b class="mono">{props.name}</b> are refused until an environment of that
          name exists again.
        </p>
        <div>
          <TypedConfirm
            name={props.name}
            label="Delete environment"
            confirm="Delete environment"
            run={async () => {
              await deleteOrgEnvironment(props.org, props.name);
              nav(environmentsHref(props.org));
            }}
          />
        </div>
      </div>
    </div>
  );
}
