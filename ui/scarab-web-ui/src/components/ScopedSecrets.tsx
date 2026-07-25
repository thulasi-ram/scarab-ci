// Secret CRUD at ONE scope (ADR-0014): list names, add, delete. Values are
// write-only — the API never returns them, so there is nothing to reveal.
//
// Scope-agnostic on purpose (ADR-0060 part B): the same panel serves the repo
// scope in the Project's Secrets tab and the org scope in global Settings. The
// scope is the caller's business; this component only renders what it is given
// and says so in its header.
import { createEffect, createResource, createSignal, For, Show } from "solid-js";
import { listSecrets, putSecret, deleteSecret, type SecretScope } from "../api/client";
import Icon from "./Icon";

export default function ScopedSecrets(props: {
  /** The scope to read and write. Changing it refetches. */
  scope: SecretScope;
  /** Panel header — names the scope in the user's words ("repo", not Project). */
  title: string;
  /** Shown when the scope holds nothing yet. */
  emptyLabel?: string;
  /** Bumping this opens the add-secret dialog (a header CTA elsewhere owns it). */
  focusPing?: number;
}) {
  const scope = () => props.scope;
  const [names, { refetch }] = createResource(scope, listSecrets);
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
        <Show when={!names.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={(names()?.length ?? 0) > 0}
            fallback={<p class="empty">{props.emptyLabel ?? "No secrets at this scope."}</p>}
          >
            <ul class="secret-list">
              <For each={names()}>
                {(n) => (
                  <li class="secret-row">
                    <Icon icon="key-round" size={15} />
                    <code class="mono">{n}</code>
                    <button
                      class="btn btn-danger btn-sm"
                      onClick={async () => { await deleteSecret(scope(), n); refetch(); }}
                    >
                      delete
                    </button>
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
          onSaved={() => refetch()}
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
  let nameRef: HTMLInputElement | undefined;
  createEffect(() => nameRef?.focus());

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
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h"><span>New secret</span></div>
        <div class="modal-body">
          <div class="form-r">
            <label>name</label>
            <input
              ref={nameRef}
              class="input"
              placeholder="NAME"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
            />
          </div>
          <div class="form-r">
            <label>value</label>
            <input
              class="input"
              type="password"
              placeholder="value (write-only)"
              value={value()}
              onInput={(e) => setValue(e.currentTarget.value)}
            />
          </div>
          <Show when={exists()}>
            <label class="secret-overwrite">
              <input
                type="checkbox"
                checked={overwrite()}
                onChange={(e) => setOverwrite(e.currentTarget.checked)}
              />
              <span>
                <code class="mono">{name().trim()}</code> already exists — overwrite it
              </span>
            </label>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button
              class="btn btn-primary"
              disabled={saving() || !name().trim() || blocked()}
              onClick={save}
            >
              <Icon icon="plus" size={14} /> {saving() ? "Saving…" : "Save"}
            </button>
            <button class="btn btn-ghost" onClick={props.onClose}>Cancel</button>
          </div>
          <p class="subtle modal-note">
            <small>Encrypted at rest, never displayed — overwrite but never read back.</small>
          </p>
        </div>
      </div>
    </div>
  );
}
