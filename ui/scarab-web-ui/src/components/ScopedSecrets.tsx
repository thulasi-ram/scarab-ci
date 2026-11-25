// The New secret dialog both key lists share (SecretKeyList: a repository's
// Secrets tab and Org settings › Secrets). Values are write-only: the API never
// returns them, so there is nothing to reveal.
import { createSignal, For, Show } from "solid-js";
import { putSecret, type SecretScope } from "../api/client";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import { Checkbox, Field, Input, Select } from "./Field";

// New-secret modal. `putSecret` is an unconditional upsert, so the overwrite
// checkbox is a client-side guard: replacing an existing name requires opting
// in, which keeps an accidental Save from silently clobbering a live secret.
//
// Given `name`, it replaces that secret: the name is filled in and read-only,
// and the same guard asks to overwrite it.
//
// Given `environments`, it also asks for the scope: the default (`""`, the
// Repository default, or the Org default when `scope` names no repo) or one
// Environment.
export function NewSecretDialog(props: {
  scope: SecretScope;
  /** The names already set at a scope (`""` is the scope itself). */
  existing: (environment: string) => string[];
  /** The Environments a value can be set in; none hides the scope field. */
  environments?: string[];
  /** The scope picked when the dialog opens; `""` is the default. */
  environment?: string;
  /** The default scope's name: "Repository default" unless told otherwise. */
  defaultLabel?: string;
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
          : "Encrypted at rest, never displayed — overwrite but never read back."
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
          help={
            props.scope.repo
              ? "The Repository default applies in every environment that does not set its own value."
              : "The Org default applies to every repository and Environment that does not set its own value. A value for one Environment applies to every repository's Runs there, unless the repository sets its own."
          }
        >
          <Select value={environment()} onValue={setEnvironment}>
            <option value="">{props.defaultLabel ?? "Repository default"}</option>
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
