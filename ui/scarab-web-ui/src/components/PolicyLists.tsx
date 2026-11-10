// The library and plugin lists: typed settings, each one Policy at the org or
// at one project. This panel is the one place each list is edited: the
// Libraries section holds the library lists, the Plugins section the plugin
// list, and a repository's Settings tab that project's own lists. The
// Policies section links here.
//
// A list is a rule without CEL: "libraries allowed here" denies a Run with a
// Step or gate from a library matching none of its sources. A new list starts
// in audit mode, so it records what it would deny before it denies anything;
// a plugin list starts enforced, since only an enforced org list lets a
// plugin's gates run.
// Adding a list needs no step-up; changing or deleting an enforced one asks
// for a passkey (the client's step-up middleware handles that).
import { createEffect, createResource, createSignal, For, Show } from "solid-js";
import { useLocation } from "@solidjs/router";
import {
  createPolicy,
  deletePolicy,
  listPolicies,
  updatePolicy,
  type PolicyRecord,
  type PolicyScope,
} from "../api/client";
import {
  appliesToProjects,
  newSettingForm,
  settingBody,
  settingFormErrors,
  settingFormOf,
  settingKindOf,
  settingLabel,
  SETTING_HELP,
  splitEntries,
  type PolicyBody,
  type SettingForm,
  type SettingKind,
} from "../policies";
import ConfirmDestructive from "./ConfirmDestructive";
import Icon from "./Icon";
import Modal, { ModalMessage, ModalSection } from "./Modal";
import { Field, Input, Select, Textarea } from "./Field";

type ListRecord = PolicyRecord & { body: PolicyBody; kind: SettingKind };

export default function PolicyLists(props: {
  org: string;
  /** A project's own lists; absent for the org's. */
  project?: string;
  kinds: readonly SettingKind[];
  /** The panel's id, which the Policies section links to. */
  anchor: string;
  title: string;
}) {
  const scope = (): PolicyScope => ({ org: props.org, project: props.project });
  const [epoch, setEpoch] = createSignal(0);
  const [policies] = createResource(
    () => ({ scope: scope(), epoch: epoch() }),
    (k) => listPolicies(k.scope),
  );
  const [editing, setEditing] = createSignal<ListRecord | SettingKind | null>(null);
  const [deleting, setDeleting] = createSignal<ListRecord | null>(null);
  const [notice, setNotice] = createSignal<string | null>(null);

  const rows = (): ListRecord[] =>
    (policies() ?? [])
      .filter((p) => !p.deleted)
      .map((p) => {
        const body = p.policy as unknown as PolicyBody;
        return { ...p, body, kind: settingKindOf(body)! };
      })
      .filter((p) => p.kind !== undefined && props.kinds.includes(p.kind))
      .sort((a, b) => a.name.localeCompare(b.name));
  const level = () => (props.project === undefined ? "org" : "project");
  // A link from the Policies section names this panel by its anchor; the
  // panel is below the section's own, which loads first, so scroll once the
  // lists are in.
  const location = useLocation();
  let panel: HTMLDivElement | undefined;
  createEffect(() => {
    if (policies.state === "ready" && location.hash === `#${props.anchor}`) {
      queueMicrotask(() => panel?.scrollIntoView({ block: "start" }));
    }
  });
  const done = (msg: string) => {
    setNotice(msg);
    setEpoch((n) => n + 1);
  };

  return (
    <div class="panel" id={props.anchor} ref={panel}>
      <div class="panel-h">
        <span>{props.title}</span>
        <Show when={rows().length > 0}>
          <span class="tok-count">{rows().length}</span>
        </Show>
      </div>
      <div class="secrets-body">
        <Show when={policies.error}>
          <p class="error">Could not load the lists.</p>
        </Show>
        <Show when={notice()}>
          <p class="notice-ok">{notice()}</p>
        </Show>
        <Show when={!policies.loading || policies.latest} fallback={<p class="empty">loading…</p>}>
          <Show when={policies.latest !== null} fallback={<p class="empty">Policies are not configured on this server.</p>}>
            <Show
              when={rows().length > 0}
              fallback={
                <p class="empty">
                  No lists{props.project === undefined ? "" : ` for ${props.project}`}:{" "}
                  {props.kinds.includes("plugins_allowed") && props.kinds.length === 1
                    ? "no plugin's gates may run until a list names its installation."
                    : "every installed library may be used."}
                </p>
              }
            >
              <ul class="tok-list" aria-label={props.title}>
                <For each={rows()}>
                  {(p) => (
                    <li class="tok">
                      <div class="tok-head">
                        <span class="tok-name mono">{p.name}</span>
                        <span class="tok-role">{settingLabel(p.kind)}</span>
                        <Show when={p.body.mode === "audit"}>
                          <span class="tok-role" title="Its denies are recorded, not enforced">
                            audit
                          </span>
                        </Show>
                        <span class="oenv-actions">
                          <button class="btn btn-ghost btn-sm" type="button" onClick={() => setEditing(p)}>
                            Edit
                          </button>
                          <button class="btn btn-ghost btn-sm" type="button" onClick={() => setDeleting(p)}>
                            Delete
                          </button>
                        </span>
                      </div>
                      <dl class="lib-facts pol-facts">
                        <Show when={level() === "org"}>
                          <dt>Applies to projects</dt>
                          <dd class="m">{appliesToProjects(p.body)}</dd>
                        </Show>
                        <dt>{p.kind === "plugins_allowed" ? "Installations" : "Sources"}</dt>
                        <dd class="m">{splitEntries(settingFormOf(p.body).entries).join(", ") || "none"}</dd>
                      </dl>
                    </li>
                  )}
                </For>
              </ul>
            </Show>
            <div class="secrets-actions">
              <For each={props.kinds}>
                {(kind) => (
                  <button class="btn btn-ghost btn-sm" type="button" onClick={() => setEditing(kind)}>
                    <Icon icon="plus" size={14} /> {settingLabel(kind)}
                  </button>
                )}
              </For>
            </div>
            <Show when={props.kinds.includes("plugins_allowed")}>
              <p class="subtle">
                <small>
                  A plugin's gates run only when an enforced org list names its installation; a
                  list in audit mode allows nothing. A project's list can only narrow the org's.
                </small>
              </p>
            </Show>
          </Show>
        </Show>
      </div>

      {/* Keyed: the dialog holds the value it opened with. Saving closes it
          (editing goes to null) and then posts the notice; a non-keyed
          accessor read during that close throws, and the notice and the
          refetch never happen. */}
      <Show when={editing()} keyed>
        {(e) => (
          <ListDialog
            scope={scope()}
            level={level()}
            record={typeof e === "string" ? undefined : e}
            initial={typeof e === "string" ? newSettingForm(e) : settingFormOf(e.body)}
            onClose={() => setEditing(null)}
            onSaved={(msg) => {
              setEditing(null);
              done(msg);
            }}
          />
        )}
      </Show>
      <Show when={deleting()} keyed>
        {(p) => (
          <ConfirmDestructive
            eyebrow={props.project === undefined ? `${props.org} · policies` : `${props.org}/${props.project} · policies`}
            title={`Delete ${p.name}?`}
            purpose={
              p.body.mode === "audit"
                ? "It is in audit mode, so nothing it enforces stops."
                : "Runs it denies are admitted again. Deleting an enforced list asks for a passkey."
            }
            name={p.name}
            confirmLabel="Delete list"
            onConfirm={async () => {
              await deletePolicy(scope(), p.id, p.version);
              done(`${p.name} is deleted; its history stays.`);
            }}
            onClose={() => setDeleting(null)}
          />
        )}
      </Show>
    </div>
  );
}

function ListDialog(props: {
  scope: PolicyScope;
  level: "org" | "project";
  record?: ListRecord;
  initial: SettingForm;
  onClose: () => void;
  onSaved: (msg: string) => void;
}) {
  const [form, setForm] = createSignal<SettingForm>(props.initial);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [tried, setTried] = createSignal(false);
  const set = <K extends keyof SettingForm>(k: K, v: SettingForm[K]) => setForm({ ...form(), [k]: v });
  const errors = () => (tried() ? settingFormErrors(form()) : {});
  const plugins = () => form().kind === "plugins_allowed";

  const save = async () => {
    setTried(true);
    if (Object.keys(settingFormErrors(form())).length > 0) return;
    setBusy(true);
    setError(null);
    try {
      const body = settingBody(form(), props.level);
      if (props.record) {
        await updatePolicy(props.scope, props.record.id, body, props.record.version);
        props.onSaved(`${body.name} is saved.`);
      } else {
        await createPolicy(props.scope, body);
        props.onSaved(`${body.name} is added${body.mode === "audit" ? " in audit mode" : ""}.`);
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : "The server refused it.");
      setBusy(false);
    }
  };

  return (
    <Modal
      eyebrow={props.scope.project === undefined ? `${props.scope.org} · policies` : `${props.scope.org}/${props.scope.project} · policies`}
      title={props.record ? `Edit ${props.record.name}` : settingLabel(form().kind)}
      purpose={SETTING_HELP[form().kind]}
      busy={busy()}
      onClose={props.onClose}
      after={
        props.record && props.record.body.mode !== "audit"
          ? { label: "Step-up", value: "Changing an enforced list asks for a passkey." }
          : undefined
      }
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={busy()}>
            Cancel
          </button>
          <button class="btn btn-primary" type="button" onClick={save} disabled={busy()}>
            {busy() ? "Saving…" : props.record ? "Save" : "Add list"}
          </button>
        </>
      }
    >
      <ModalSection n={1} title="List">
        <Field label="Name" required error={errors().name} help="Letters, digits, -, _ or .">
          <Input mono value={form().name} onValue={(v) => set("name", v)} disabled={!!props.record} />
        </Field>
        <Field
          label={plugins() ? "Installations" : "Sources"}
          required
          error={errors().entries}
          help={
            plugins()
              ? "One plugin installation name per line, exactly as installed."
              : "One per line: an exact library source, or a prefix ending in *, such as github.com/acme/*."
          }
        >
          <Textarea mono rows={5} value={form().entries} onValue={(v) => set("entries", v)} />
        </Field>
      </ModalSection>
      <Show when={props.level === "org"}>
        <ModalSection n={2} title="Applies to">
          <Field label="Projects" help="Comma-separated; * matches any run of characters. Empty: every project.">
            <Input mono value={form().projects} onValue={(v) => set("projects", v)} />
          </Field>
          <Field label="Except projects" help="Projects the list does not apply to, even when they match above.">
            <Input mono value={form().exceptProjects} onValue={(v) => set("exceptProjects", v)} />
          </Field>
        </ModalSection>
      </Show>
      <ModalSection n={props.level === "org" ? 3 : 2} title="Mode">
        <Field
          label="Mode"
          help="Audit records what the list would deny and denies nothing. Enforce denies."
        >
          <Select value={form().mode} onValue={(v) => set("mode", v as SettingForm["mode"])}>
            <option value="audit">audit</option>
            <option value="enforce">enforce</option>
          </Select>
        </Field>
      </ModalSection>
    </Modal>
  );
}
