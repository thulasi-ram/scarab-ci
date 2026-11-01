// Org-level Environments: the Settings section where an org admin runs the
// places deploys land in. Each Environment has:
//
//   - its description;
//   - its BASELINE protection rules, which every project's Deploy Runs obey;
//   - its ELIGIBILITY: all projects, or project-name patterns. An empty list
//     admits no project: eligibility starts closed;
//   - per-project PRIVILEGE GRANTS (image digests, raw k8s_overlay), granted
//     to one project, never to the Environment as a whole.
//
// This section is the list and the way to create one. Each name links to the
// Environment's own read-only page, and every change to it is on that page's
// edit screen (`routes/EnvironmentPage.tsx`, git-bug c11abc5).
//
// Above the list sits the migration's SPLIT REPORT: every name the migration
// merged across projects, or opened to all projects, with each project's old
// rules beside its overlay now, and a banner counting what is left to review
// and which baselines are still permissive. Splitting a name that is really
// two places is a YAML change in the projects concerned; the panel says so.
//
// Every server refusal (400/409) is shown verbatim: it is the answer to "why
// will this not save".
import { createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { Portal } from "solid-js/web";
import { A, useNavigate } from "@solidjs/router";
import {
  acknowledgeMigrationReport,
  getMigrationReport,
  listOrgEnvironments,
  putOrgEnvironment,
  type MigrationReportLine,
  type OrgEnvironment,
} from "../api/client";
import { eligibilityLabel, isPermissive, reportBanner, rulesSummary } from "../environment-overlays";
import { environmentHref, nameError, toPutBody } from "../org-environments";
import { relTime, absTime } from "../fmt";
import Icon from "./Icon";

export default function OrgEnvironments(props: { org: string }) {
  const nav = useNavigate();
  const [epoch, setEpoch] = createSignal(0);
  const [environments] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listOrgEnvironments(k.org),
  );
  const [creating, setCreating] = createSignal(false);
  const [report, { refetch: refetchReport }] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => getMigrationReport(k.org).catch(() => null),
  );

  // `null` is the server answering "no org-Environment store here", which is a
  // different sentence from "none registered".
  const unavailable = () => !environments.loading && !environments.error && environments() === null;
  const rows = () => environments() ?? [];

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
                <For each={rows()}>{(env) => <EnvironmentRow org={props.org} env={env} />}</For>
              </ul>
            </Show>

            <div class="secrets-actions">
              <button class="btn btn-primary btn-sm" onClick={() => setCreating(true)}>
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

      <Show when={creating()}>
        {/* Portalled, as Plugins' install dialog: in-panel, it rendered under
            the panels after this one (each is its own stacking context). */}
        <Portal>
          <CreateDialog
            org={props.org}
            taken={rows().map((e) => e.name)}
            onClose={() => setCreating(false)}
            onCreated={(name) => {
              setCreating(false);
              setEpoch((e) => e + 1);
              nav(environmentHref(props.org, name));
            }}
          />
        </Portal>
      </Show>
    </div>
  );
}

/** One registered Environment: its name (a link to its page), rules, and who
 *  last changed it. No controls: changes are on the Environment's page. */
function EnvironmentRow(props: { org: string; env: OrgEnvironment }) {
  const env = () => props.env;
  return (
    <li class="tok">
      <div class="tok-head">
        <Icon icon="rocket" size={15} />
        <A href={environmentHref(props.org, env().name)} class="tok-name mono oenv-link">
          {env().name}
        </A>
        <Show when={isPermissive(env().protection)}>
          <span class="tok-role" title="No org-wide rule protects this place yet, only each project's own overlay">
            permissive baseline
          </span>
        </Show>
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

/** Create an Environment: its name and description. The name is fixed once it
 *  exists: it is the key every pipeline's `environment:` names. The rest is
 *  set on its edit page. */
function CreateDialog(props: {
  org: string;
  /** Names already registered: a create must not silently replace one. */
  taken: string[];
  onClose: () => void;
  onCreated: (name: string) => void;
}) {
  const [name, setName] = createSignal("");
  const [description, setDescription] = createSignal("");
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") props.onClose();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  const nameProblem = () => {
    const n = name();
    if (props.taken.includes(n)) return `${n} is already registered. Open it to change it.`;
    return nameError(n);
  };

  const save = async () => {
    const body = toPutBody({ description: description() }).body;
    if (nameProblem() || saving()) return;
    setSaving(true);
    setError(null);
    try {
      await putOrgEnvironment(props.org, name(), body);
      props.onCreated(name());
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not create the environment.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal modal-tall" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h">
          <span>New environment</span>
        </div>
        <div class="modal-body token-form">
          <div class="form-r">
            <label for="new-env-name">name</label>
            <input
              id="new-env-name"
              class="input mono"
              placeholder="production, staging, prod-eu"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
              autofocus
            />
            <p class="fieldnote">
              Exactly the name pipelines use; case matters. It cannot be changed later.
            </p>
          </div>

          <div class="form-r">
            <label for="new-env-description">description</label>
            <input
              id="new-env-description"
              class="input"
              placeholder="what lands here, who runs it"
              value={description()}
              onInput={(e) => setDescription(e.currentTarget.value)}
            />
          </div>
          <p class="fieldnote">
            A new environment has no baseline rule and admits no project. Set both on its page.
          </p>
        </div>

        <div class="modal-foot">
          <Show when={name() && nameProblem()}>
            <p class="error">{nameProblem()}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={!!nameProblem() || saving()} onClick={save}>
              {saving() ? "Creating…" : "Create environment"}
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

/** The migration's split report: each name merged
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
