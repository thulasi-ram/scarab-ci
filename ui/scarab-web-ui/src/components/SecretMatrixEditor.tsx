// The secret coverage matrix as an EDITOR.
//
// Rows are keys, columns are scopes: `Repository default` (the repo scope every
// environment falls through to) then one per Environment. Clicking a cell edits
// the value AT THAT SCOPE — so the same grid that shows you a gap is the thing
// that fills it. Before this, an environment-scoped secret had no UI at all.
//
// Secrets are one key-addressed namespace resolved by scope, which
// is why this is one grid rather than a per-environment editor: the Environments
// tab stays rules-only, and "does prod override the default?" is a question you
// answer by looking along a row.
//
// Values are write-only — nothing here can display one. A cell shows only which
// scope a value lives at:
//
//   ●  set here      a value at exactly this scope
//   ○  inherited     none here; resolves from repo/org (the tooltip says which)
//   –  unset         resolves to nothing
//   ⊘  silenced      unset ON PURPOSE (a marker) — advisory, never blocks
import { createResource, createSignal, For, Show } from "solid-js";
import {
  fetchSecretMatrix,
  putSecret,
  deleteSecret,
  setSecretCellSilenced,
  columnScope,
  REPO_DEFAULT_COLUMN,
  type SecretCellStatus,
} from "../api/client";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import { Checkbox, Field, Input } from "./Field";

const CELL: Record<SecretCellStatus, { glyph: string; label: string }> = {
  set: { glyph: "●", label: "set at this scope" },
  inherited: { glyph: "○", label: "inherited" },
  unset: { glyph: "–", label: "unset" },
  silenced: { glyph: "⊘", label: "intentionally unset" },
};

/** The column header. The repo default is named in the user's words: "repo". */
const columnLabel = (column: string) =>
  column === REPO_DEFAULT_COLUMN ? "Repository default" : column;

type Editing = { key: string; column: string };

export default function SecretMatrixEditor(props: {
  org: string;
  repo: string;
  /** Bumping this refetches — the repo-scope panel above added or deleted a key. */
  epoch?: number;
  /** Called after a cell write, so the repo-scope panel can refresh too. */
  onChange?: () => void;
}) {
  const key = () => ({ org: props.org, repo: props.repo, epoch: epoch(), outer: props.epoch ?? 0 });
  const [epoch, setEpoch] = createSignal(0);
  const [matrix] = createResource(key, ({ org, repo }) => fetchSecretMatrix(org, repo));
  const [editing, setEditing] = createSignal<Editing | null>(null);
  const reload = () => {
    setEpoch((e) => e + 1);
    props.onChange?.();
  };

  const rowOf = (k: string) => matrix()?.keys.find((r) => r.key === k);
  const statusOf = (k: string, column: string): SecretCellStatus =>
    rowOf(k)?.status[column] ?? "unset";

  return (
    <div class="panel">
      <div class="panel-h"><span>Secrets by scope</span></div>
      <div class="secrets-body">
        <Show when={!matrix.loading} fallback={<p class="empty">loading…</p>}>
          <Show when={matrix.error}>
            <p class="error">Could not load the secret matrix.</p>
          </Show>
          <Show
            when={(matrix()?.keys.length ?? 0) > 0}
            fallback={
              <p class="empty">
                No secrets yet. Add one above, then set per-environment overrides here.
              </p>
            }
          >
            <div class="matrix-scroll">
              <table class="secret-matrix editable">
                <thead>
                  <tr>
                    <th class="mkey">key</th>
                    <For each={matrix()!.columns}>
                      {(c) => (
                        <th classList={{ mdefault: c === REPO_DEFAULT_COLUMN }}>
                          {columnLabel(c)}
                        </th>
                      )}
                    </For>
                  </tr>
                </thead>
                <tbody>
                  <For each={matrix()!.keys}>
                    {(row) => (
                      <tr>
                        <td class="mkey"><code class="mono">{row.key}</code></td>
                        <For each={matrix()!.columns}>
                          {(c) => {
                            const s = () => (row.status[c] ?? "unset") as SecretCellStatus;
                            const from = () => row.inherited_from[c];
                            const title = () =>
                              s() === "inherited"
                                ? `inherited from the ${from() === "org" ? "org" : "repo default"}`
                                : CELL[s()].label;
                            return (
                              <td
                                class={`mcell m-${s()}`}
                                title={`${title()} — click to edit`}
                                onClick={() => setEditing({ key: row.key, column: c })}
                              >
                                {CELL[s()].glyph}
                              </td>
                            );
                          }}
                        </For>
                      </tr>
                    )}
                  </For>
                </tbody>
              </table>
            </div>
            <p class="subtle">
              <small>
                After inheritance: <b>●</b> set at this scope · <b>○</b> inherited (hover to
                see from where) · <b>–</b> unset · <b>⊘</b> intentionally unset. Click any
                cell to set or clear a value <i>at that scope</i>. Coverage is advisory — a
                gap never blocks a deploy.
              </small>
            </p>
          </Show>
        </Show>
      </div>
      <Show when={editing()}>
        {(cell) => (
          <CellEditor
            org={props.org}
            repo={props.repo}
            cell={cell()}
            status={statusOf(cell().key, cell().column)}
            inheritedFrom={rowOf(cell().key)?.inherited_from[cell().column]}
            onClose={() => setEditing(null)}
            onSaved={reload}
          />
        )}
      </Show>
    </div>
  );
}

/**
 * Edit ONE cell. Three actions, and which are offered depends on the cell:
 * set/replace a value at this scope, clear the value here (falling back to the
 * broader scope), or toggle the advisory "intentionally unset" marker — which is
 * only meaningful for a cell that genuinely resolves to nothing.
 */
function CellEditor(props: {
  org: string;
  repo: string;
  cell: Editing;
  status: SecretCellStatus;
  inheritedFrom?: "repo" | "org";
  onClose: () => void;
  onSaved: () => void;
}) {
  const [value, setValue] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const scope = () => columnScope(props.org, props.repo, props.cell.column);
  const isDefaultColumn = () => props.cell.column === REPO_DEFAULT_COLUMN;

  const run = async (op: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await op();
      props.onSaved();
      props.onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not apply the change.");
    } finally {
      setBusy(false);
    }
  };

  const save = () =>
    run(() => putSecret({ ...scope(), name: props.cell.key, value: value() }));
  const clear = () => run(() => deleteSecret(scope(), props.cell.key));
  const toggleSilenced = () =>
    run(() =>
      setSecretCellSilenced(
        props.org,
        props.repo,
        props.cell.column,
        props.cell.key,
        props.status !== "silenced",
      ),
    );

  /** What clearing this cell would fall back to — stated, never implied. */
  const fallback = () =>
    isDefaultColumn() ? "the org-wide value, if any" : "the repository default";

  return (
    <Modal
      eyebrow={`${props.org}/${props.repo} · secrets`}
      title={
        <>
          <span class="mono">{props.cell.key}</span> · {columnLabel(props.cell.column)}
        </>
      }
      purpose={`Encrypted at rest, never displayed. Clearing here falls back to ${fallback()}.`}
      busy={busy()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <Show when={props.status === "set"}>
            <button class="btn btn-danger" type="button" disabled={busy()} onClick={clear}>
              Clear here
            </button>
            <span class="sp" />
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={busy()}>
            Cancel
          </button>
          <button class="btn btn-primary" type="button" disabled={busy() || !value()} onClick={save}>
            <Icon icon="key-round" size={14} />{" "}
            {busy() ? "Saving…" : props.status === "set" ? "Replace" : "Set here"}
          </button>
        </>
      }
    >
      <p class="dlg-text cell-state">
        <Show when={props.status === "set"}>
          A value is set at this scope. Saving replaces it.
        </Show>
        <Show when={props.status === "inherited"}>
          Currently inherited from the{" "}
          {props.inheritedFrom === "org" ? "org" : "repository default"}. Saving
          <b> overrides it here only</b>.
        </Show>
        <Show when={props.status === "unset" || props.status === "silenced"}>
          Nothing resolves here today. Saving defines it at this scope.
        </Show>
      </p>
      <Field label="Value">
        <Input mono autofocus type="password" placeholder="value (write-only)" value={value()} onValue={setValue} />
      </Field>
      <Show when={props.status !== "set" && props.status !== "inherited"}>
        <Checkbox
          tone="warn"
          checked={props.status === "silenced"}
          disabled={busy()}
          onChecked={() => void toggleSilenced()}
          label="Intentionally unset"
          help="Silence this cell so the gap doesn't read as an oversight. Advisory only."
        />
      </Show>
    </Modal>
  );
}
