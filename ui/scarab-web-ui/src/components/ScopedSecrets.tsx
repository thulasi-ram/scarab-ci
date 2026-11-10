// Secret CRUD at ONE scope: list names, add, replace, delete. Values are
// write-only: the API never returns them, so there is nothing to reveal.
//
// The org scope in Organization settings › Secrets uses it: New secret at the
// top right, and one row per name with Replace (the same dialog with the name
// filled in) and Delete, which asks for the name to be typed. A repository's
// Secrets tab has its own key list (SecretKeyList), which shares the New
// secret dialog below.
import { createResource, createSignal, For, Show, type JSX } from "solid-js";
import { listSecrets, putSecret, deleteSecret, type SecretScope } from "../api/client";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import ConfirmDestructive from "./ConfirmDestructive";
import { Checkbox, Field, Input, Select } from "./Field";

export default function ScopedSecrets(props: {
  /** The scope to read and write. Changing it refetches. */
  scope: SecretScope;
  /** Panel header: names the scope in the user's words. */
  title: string;
  /** Shown when the scope holds nothing yet. */
  emptyLabel?: string;
  /** More for the note under the list, after the sentence on values. */
  note?: JSX.Element;
}) {
  const scope = () => props.scope;
  const [names, { refetch }] = createResource(
    () => props.scope,
    async (s) => (await listSecrets(s)).slice().sort(),
  );
  // The dialog: `""` for a new secret, a name to replace that one.
  const [editing, setEditing] = createSignal<string | null>(null);
  const [deleting, setDeleting] = createSignal<string | null>(null);
  const where = () => (props.scope.repo ? `${props.scope.org}/${props.scope.repo}` : props.scope.org);

  return (
    <div class="panel">
      <div class="panel-h">
        <span>{props.title}</span>
        <button class="btn btn-primary btn-sm" onClick={() => setEditing("")}>
          <Icon icon="plus" size={14} /> New secret
        </button>
      </div>
      <div class="secrets-body">
        <Show when={names.error}>
          <p class="error">Could not load the secrets.</p>
        </Show>
        <Show when={!names.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={(names()?.length ?? 0) > 0}
            fallback={
              <Show when={!names.error}>
                <p class="empty">{props.emptyLabel ?? "No secrets at this scope."}</p>
              </Show>
            }
          >
            <ul class="secret-list" aria-label="Secrets">
              <For each={names()}>
                {(name) => (
                  <li class="secret-row">
                    <Icon icon="key-round" size={15} />
                    <code class="mono">{name}</code>
                    <span class="oenv-actions">
                      <button class="btn btn-ghost btn-sm" onClick={() => setEditing(name)}>
                        Replace
                      </button>
                      <button class="btn btn-danger btn-sm" onClick={() => setDeleting(name)}>
                        Delete
                      </button>
                    </span>
                  </li>
                )}
              </For>
            </ul>
          </Show>
          <p class="subtle">
            <small>
              <Icon icon="key-round" size={12} /> Values are never shown. Setting a name again
              replaces it. {props.note}
            </small>
          </p>
        </Show>
      </div>
      <Show when={editing() !== null}>
        <NewSecretDialog
          scope={scope()}
          name={editing() || undefined}
          existing={() => names() ?? []}
          onClose={() => setEditing(null)}
          onSaved={() => refetch()}
        />
      </Show>
      <Show when={deleting()}>
        {(name) => (
          <ConfirmDestructive
            eyebrow={`${where()} · secrets`}
            title={`Delete ${name()}?`}
            purpose="Its value cannot be recovered."
            name={name()}
            confirmLabel="Delete secret"
            onConfirm={async () => {
              await deleteSecret(scope(), name());
              void refetch();
            }}
            onClose={() => setDeleting(null)}
          >
            <p>
              A repository or Environment that sets its own value keeps it. Everywhere else, Steps
              that read it get nothing.
            </p>
          </ConfirmDestructive>
        )}
      </Show>
    </div>
  );
}

// New-secret modal. `putSecret` is an unconditional upsert, so the overwrite
// checkbox is a client-side guard: replacing an existing name requires opting
// in, which keeps an accidental Save from silently clobbering a live secret.
//
// Given `name`, it replaces that secret: the name is filled in and read-only,
// and the same guard asks to overwrite it.
//
// Given `environments`, it also asks for the scope: the Repository default
// (`""`) or one Environment of the repository in `scope`.
export function NewSecretDialog(props: {
  scope: SecretScope;
  /** The names already set at a scope (`""` is the scope itself). */
  existing: (environment: string) => string[];
  /** The Environments a value can be set in; none hides the scope field. */
  environments?: string[];
  /** The scope picked when the dialog opens; `""` is the Repository default. */
  environment?: string;
  /** Replace this secret: its name, filled in and read-only. */
  name?: string;
  onClose: () => void;
  onSaved: () => void;
}) {
  // The name to replace is read once, on open.
  const [name, setName] = createSignal(props.name ?? "");
  const [value, setValue] = createSignal("");
  // The initial pick is read once, on open.
  const [environment, setEnvironment] = createSignal(props.environment ?? "");
  const [overwrite, setOverwrite] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const target = (): SecretScope =>
    environment() ? { ...props.scope, environment: environment() } : props.scope;
  const exists = () => props.existing(environment()).includes(name().trim());
  const blocked = () => exists() && !overwrite();
  const where = () =>
    [props.scope.repo ? `${props.scope.org}/${props.scope.repo}` : props.scope.org, target().environment]
      .filter(Boolean)
      .join(" · ");

  const save = async () => {
    const n = name().trim();
    if (!n || blocked()) return;
    setSaving(true);
    setError(null);
    try {
      await putSecret({ ...target(), name: n, value: value() });
      props.onSaved();
      props.onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save secret.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      eyebrow={`${where()} · secrets`}
      title={props.name ? "Replace secret" : "New secret"}
      purpose={
        props.name
          ? "The new value replaces the old one, which cannot be read back."
          : "Stored encrypted and never shown after you save it."
      }
      busy={saving()}
      onClose={props.onClose}
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
            disabled={saving() || !name().trim() || blocked()}
            onClick={save}
          >
            <Icon icon="plus" size={14} /> {saving() ? "Saving…" : "Save"}
          </button>
        </>
      }
    >
      <Field label="Name" required>
        <Input
          mono
          autofocus={!props.name}
          readOnly={!!props.name}
          placeholder="NAME"
          value={name()}
          onValue={setName}
        />
      </Field>
      <Field label="Value">
        <Input
          mono
          autofocus={!!props.name}
          type="password"
          placeholder="value (write-only)"
          value={value()}
          onValue={setValue}
        />
      </Field>
      <Show when={(props.environments?.length ?? 0) > 0}>
        <Field
          label="Scope"
          help="The Repository default applies in every environment that does not set its own value."
        >
          <Select value={environment()} onValue={setEnvironment}>
            <option value="">Repository default</option>
            <For each={props.environments}>{(e) => <option value={e}>{e}</option>}</For>
          </Select>
        </Field>
      </Show>
      <Show when={exists()}>
        <Checkbox
          tone="warn"
          checked={overwrite()}
          onChecked={setOverwrite}
          label={
            <>
              <code class="mono">{name().trim()}</code> is already set{" "}
              {environment() ? `in ${environment()}` : "here"}. Overwrite it
            </>
          }
        />
      </Show>
    </Modal>
  );
}
