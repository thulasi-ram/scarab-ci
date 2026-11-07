// Secret CRUD at ONE scope: list names, add, delete. Values are
// write-only — the API never returns them, so there is nothing to reveal.
//
// Scope-agnostic on purpose: the same panel serves the repo
// scope in the Project's Secrets tab and the org scope in global Settings. The
// scope is the caller's business; this component only renders what it is given
// and says so in its header.
//
// At the repo scope it also lists, greyed out, the keys the repo can use but
// does not hold: the org's keys and keys set only in some environments. They
// come from the secret matrix and cannot be edited here.
import { createEffect, createResource, createSignal, For, Show } from "solid-js";
import {
  listSecrets,
  putSecret,
  deleteSecret,
  fetchSecretMatrix,
  type SecretScope,
} from "../api/client";
import { secretKeyNote, secretKeyRows } from "../secret-keys";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import { Checkbox, Field, Input } from "./Field";

export default function ScopedSecrets(props: {
  /** The scope to read and write. Changing it refetches. */
  scope: SecretScope;
  /** Panel header — names the scope in the user's words ("repo", not Project). */
  title: string;
  /** Shown when the scope holds nothing yet. */
  emptyLabel?: string;
  /** Bumping this opens the add-secret dialog (a header CTA elsewhere owns it). */
  focusPing?: number;
  /** Bumping this refetches — another panel wrote to the same scope. */
  epoch?: number;
  /** Called after this panel writes, so a sibling view of the scope can refresh. */
  onChange?: () => void;
  /** Also list the keys this repo inherits (repo scope only). */
  showInherited?: boolean;
}) {
  const scope = () => props.scope;
  const [data, { refetch }] = createResource(
    () => ({ scope: props.scope, inherited: props.showInherited ?? false, epoch: props.epoch ?? 0 }),
    async (k) => {
      const repo = k.scope.repo;
      const [names, matrix] = await Promise.all([
        listSecrets(k.scope),
        // The list still works without the inherited keys, so a failed matrix
        // read leaves just the repo's own.
        k.inherited && repo
          ? fetchSecretMatrix(k.scope.org, repo).catch(() => null)
          : Promise.resolve(null),
      ]);
      return { names, rows: secretKeyRows(names, matrix) };
    },
  );
  const names = () => data()?.names;
  const changed = () => {
    refetch();
    props.onChange?.();
  };
  const [showDialog, setShowDialog] = createSignal(false);

  // The header "New secret" CTA pings us to open the add-secret modal. Guard on
  // > 0 so we don't pop the dialog on the initial mount.
  createEffect(() => {
    if ((props.focusPing ?? 0) > 0) setShowDialog(true);
  });

  return (
    <div class="panel">
      <div class="panel-h"><span>{props.title}</span></div>
      <div class="secrets-body">
        <Show when={!data.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={(data()?.rows.length ?? 0) > 0}
            fallback={<p class="empty">{props.emptyLabel ?? "No secrets at this scope."}</p>}
          >
            <ul class="secret-list">
              <For each={data()?.rows}>
                {(row) => (
                  <li
                    class="secret-row"
                    classList={{ "secret-inherited": row.kind !== "own" }}
                    title={
                      row.kind === "own"
                        ? undefined
                        : "Not set at this scope. Add a secret with this name to set it here."
                    }
                  >
                    <Icon icon="key-round" size={15} />
                    <code class="mono">{row.name}</code>
                    <Show when={secretKeyNote(row)}>
                      <span class="secret-note">{secretKeyNote(row)}</span>
                    </Show>
                    <Show when={row.kind === "own"}>
                      <button
                        class="btn btn-danger btn-sm"
                        onClick={async () => { await deleteSecret(scope(), row.name); changed(); }}
                      >
                        delete
                      </button>
                    </Show>
                  </li>
                )}
              </For>
            </ul>
          </Show>
          <div class="secrets-actions">
            <button class="btn btn-primary btn-sm" onClick={() => setShowDialog(true)}>
              <Icon icon="plus" size={14} /> New secret
            </button>
          </div>
          <p class="subtle"><small>Encrypted at rest, never displayed — overwrite but never read back.</small></p>
        </Show>
      </div>
      <Show when={showDialog()}>
        <NewSecretDialog
          scope={scope()}
          existing={names() ?? []}
          onClose={() => setShowDialog(false)}
          onSaved={changed}
        />
      </Show>
    </div>
  );
}

// New-secret modal. `putSecret` is an unconditional upsert, so the overwrite
// checkbox is a client-side guard: replacing an existing name requires opting
// in, which keeps an accidental Save from silently clobbering a live secret.
export function NewSecretDialog(props: {
  scope: SecretScope;
  existing: string[];
  onClose: () => void;
  onSaved: () => void;
}) {
  const [name, setName] = createSignal("");
  const [value, setValue] = createSignal("");
  const [overwrite, setOverwrite] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const exists = () => props.existing.includes(name().trim());
  const blocked = () => exists() && !overwrite();
  const where = () =>
    [props.scope.repo ? `${props.scope.org}/${props.scope.repo}` : props.scope.org, props.scope.environment]
      .filter(Boolean)
      .join(" · ");

  const save = async () => {
    const n = name().trim();
    if (!n || blocked()) return;
    setSaving(true);
    setError(null);
    try {
      await putSecret({ ...props.scope, name: n, value: value() });
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
      title="New secret"
      purpose="Encrypted at rest, never displayed — overwrite but never read back."
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
        <Input mono autofocus placeholder="NAME" value={name()} onValue={setName} />
      </Field>
      <Field label="Value">
        <Input mono type="password" placeholder="value (write-only)" value={value()} onValue={setValue} />
      </Field>
      <Show when={exists()}>
        <Checkbox
          tone="warn"
          checked={overwrite()}
          onChecked={setOverwrite}
          label={
            <>
              <code class="mono">{name().trim()}</code> already exists — overwrite it
            </>
          }
        />
      </Show>
    </Modal>
  );
}
