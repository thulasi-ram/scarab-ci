// Org-level Environments: the Settings section where an org admin runs the
// places deploys land in. Each Environment has:
//
//   - its display name (what lists and headings show; the name, which
//     pipelines use, is fixed) and its description;
//   - its BASELINE protection rules, which every project's Deploy Runs obey;
//   - its ELIGIBILITY: all projects, or project-name patterns. An empty list
//     admits no project: eligibility starts closed;
//   - per-project PRIVILEGE GRANTS (image digests, raw k8s_overlay), granted
//     to one project, never to the Environment as a whole.
//
// This section is the list and the way to create one: New environment, top
// right, opens a dialog for its name. Each row links to the Environment's
// manage page (`…/environments/<name>/manage`, routes/EnvironmentPage.tsx,
// git-bug c11abc5), which holds every change to it.
//
// Every server refusal (400/409) is shown verbatim: it is the answer to "why
// will this not save".
import { createResource, createSignal, For, Show } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import { listOrgEnvironments, putOrgEnvironment, type OrgEnvironment } from "../api/client";
import {
  eligibilityLabel,
  isPermissive,
  rulesSummary,
  unprotectedBanner,
  unprotectedButOpen,
} from "../environment-overlays";
import {
  displayNameError,
  environmentHref,
  nameBeside,
  nameError,
  shownName,
  toPutBody,
} from "../org-environments";
import { relTime, absTime } from "../fmt";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import { Field, Input } from "./Field";

export default function OrgEnvironments(props: { org: string }) {
  const nav = useNavigate();
  const [epoch, setEpoch] = createSignal(0);
  const [environments] = createResource(
    () => ({ org: props.org, epoch: epoch() }),
    (k) => listOrgEnvironments(k.org),
  );
  const [creating, setCreating] = createSignal(false);

  // `null` is the server answering "no org-Environment store here", which is a
  // different sentence from "none registered".
  const unavailable = () => !environments.loading && !environments.error && environments() === null;
  const rows = () => environments() ?? [];

  return (
    <div class="panel">
      <div class="panel-h">
        <span>
          Environments · {props.org} <span class="tok-count">{rows().length}</span>
        </span>
        <Show when={!unavailable() && !environments.error}>
          <button class="btn btn-primary btn-sm" onClick={() => setCreating(true)}>
            <Icon icon="plus" size={14} /> New environment
          </button>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={environments.error}>
          <p class="error">Could not load the org's environments.</p>
        </Show>

        <Show when={!environments.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={!unavailable()}
            fallback={<p class="empty">This server does not keep org-level environments.</p>}
          >
            <Show when={rows().length > 0} fallback={<p class="empty">No environments in {props.org} yet.</p>}>
              <Show when={unprotectedBanner(rows())}>
                {(w) => (
                  <p class="tok-warn env-warn" role="status">
                    <Icon icon="alert-triangle" size={14} /> {w()}
                  </p>
                )}
              </Show>
              <ul class="tok-list" aria-label="Environments">
                <For each={rows()}>{(env) => <EnvironmentRow org={props.org} env={env} />}</For>
              </ul>
            </Show>

            <p class="subtle">
              <small>
                <Icon icon="rocket" size={12} /> An environment is where deploys land, shared by
                every repository that deploys there. A pipeline's{" "}
                <span class="mono">environment:</span> is refused until an environment of that name
                exists here and lets the project use it. Each environment decides which projects may
                use it and under which rules; a repository can only tighten those for itself.
              </small>
            </p>
          </Show>
        </Show>
      </div>

      <Show when={creating()}>
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
      </Show>
    </div>
  );
}

/** One registered Environment: its name, rules, who last changed it, and a
 *  link to its manage page, where every change is made. */
function EnvironmentRow(props: { org: string; env: OrgEnvironment }) {
  const env = () => props.env;
  return (
    <li class="tok">
      <div class="tok-head">
        <Icon icon="rocket" size={15} />
        <A
          href={environmentHref(props.org, env().name)}
          class="tok-name oenv-link"
          classList={{ mono: nameBeside(env()) === null }}
        >
          {shownName(env())}
        </A>
        <Show when={nameBeside(env())}>
          {(name) => (
            <span class="oenv-name mono" title="The name pipelines use">
              {name()}
            </span>
          )}
        </Show>
        <Show when={unprotectedButOpen(env())}>
          <span class="tok-role" title="Projects may deploy here and no org-wide rule protects it, only each project's own overlay">
            permissive baseline
          </span>
        </Show>
        <span class="oenv-actions">
          <A class="btn btn-ghost btn-sm" href={environmentHref(props.org, env().name)}>
            Manage
          </A>
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
        <span title="Eligibility: the projects that may use it">
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

/** Create an Environment: its name, display name and description. The name is
 *  fixed once it exists: it is the key every pipeline's `environment:` names.
 *  The rest is set on its manage page. */
function CreateDialog(props: {
  org: string;
  /** Names already registered: a create must not silently replace one. */
  taken: string[];
  onClose: () => void;
  onCreated: (name: string) => void;
}) {
  const [name, setName] = createSignal("");
  const [displayName, setDisplayName] = createSignal("");
  const [description, setDescription] = createSignal("");
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const nameProblem = () => {
    const n = name();
    if (props.taken.includes(n)) return `${n} is already registered. Open it to change it.`;
    return nameError(n);
  };
  const displayProblem = () => displayNameError(displayName());

  const save = async () => {
    const body = toPutBody({ displayName: displayName(), description: description() }).body;
    if (nameProblem() || displayProblem() || saving()) return;
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
    <Modal
      eyebrow={`${props.org} · environments`}
      title="New environment"
      purpose="A place deploys land, under the name pipelines use for it."
      busy={saving()}
      onClose={props.onClose}
      after={{
        label: "After creating",
        value: "A new environment has no baseline rule and admits no project. Set both on its page.",
      }}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={saving()}>
            Cancel
          </button>
          <button
            class="btn btn-primary"
            type="button"
            disabled={!!nameProblem() || !!displayProblem() || saving()}
            onClick={save}
          >
            {saving() ? "Creating…" : "Create environment"}
          </button>
        </>
      }
    >
      <Field
        id="new-env-name"
        label="Name"
        required
        help="Exactly the name pipelines use; case matters. It cannot be changed later."
        error={name() ? nameProblem() : null}
      >
        <Input mono autofocus placeholder="production, staging, prod-eu" value={name()} onValue={setName} />
      </Field>
      <Field
        id="new-env-display-name"
        label="Display name"
        help="What lists and headings show. Blank shows the name. Can be changed later."
        error={displayProblem()}
      >
        <Input placeholder="Production (EU)" value={displayName()} onValue={setDisplayName} />
      </Field>
      <Field id="new-env-description" label="Description">
        <Input placeholder="what lands here, who runs it" value={description()} onValue={setDescription} />
      </Field>
    </Modal>
  );
}
