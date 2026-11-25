// A repository's secrets, or the org's, as a key list. Each key is one row with
// a summary of where it is set; expanding it shows one line per scope: the
// Repository default (the repo scope every Environment falls back to), then
// each Environment. Without a repo it is the org's list (Org settings ›
// Secrets): the Org default, then the org's value for each Environment, each
// line saying which repositories set their own in front of it. Inherited and unset Environments fold behind a "N more" line, so
// the list stays readable with many Environments.
//
// Picking an Environment makes every row's summary say what a Run in that
// Environment gets for the key, and where it comes from.
//
// Values are write-only: nothing here can display one. A line shows only which
// scope a value lives at:
//
//   ●  set here          a value at exactly this scope
//   ○  inherited         none here; resolves from the repo default or the org
//   –  unset             resolves to nothing
//   ⊘  unset on purpose  a marker for a gap that is meant to be there; advisory
import { createResource, createSignal, createUniqueId, For, Show } from "solid-js";
import {
  fetchOrgSecretMatrix,
  fetchSecretMatrix,
  listSecrets,
  putSecret,
  deleteSecret,
  setSecretCellSilenced,
  columnScope,
  REPO_DEFAULT_COLUMN,
  type SecretMatrix,
} from "../api/client";
import {
  GLYPH,
  envSummary,
  foldLabel,
  isOrgKey,
  keySummary,
  matrixFromNames,
  namesSetAt,
  scopeColumns,
  scopeLabel,
  scopeLines,
  sortedKeys,
  stateText,
  statusAt,
  type SecretKeyRow,
} from "../secret-keys";
import Icon from "./Icon";
import Modal, { ModalMessage } from "./Modal";
import { Checkbox, Field, Input } from "./Field";
import SearchSelect from "./SearchSelect";
import { NewSecretDialog } from "./ScopedSecrets";

type Cell = { key: string; column: string };

/** A write in flight: its busy flag, its error, and a runner that sets both. */
function useWrite(onDone: () => void) {
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const run = async (op: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await op();
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not apply the change.");
    } finally {
      setBusy(false);
    }
  };
  return { busy, error, run };
}

/** Add or remove one name in a set held by a signal. */
const toggleIn = (set: ReadonlySet<string>, name: string) => {
  const next = new Set(set);
  if (!next.delete(name)) next.add(name);
  return next;
};

export default function SecretKeyList(props: {
  org: string;
  /** The repository; none is the org's own list. */
  repo?: string;
  /** The picked Environment; `""` is all of them. */
  environment: string;
  onEnvironment: (environment: string) => void;
}) {
  // The matrix carries every key the repo can resolve. Without it (a failed
  // read), the list falls back to the repo scope's own names.
  const isOrg = () => !props.repo;
  const [data, { refetch }] = createResource(
    () => ({ org: props.org, repo: props.repo }),
    async (k): Promise<{ matrix: SecretMatrix; partial: boolean }> => {
      try {
        const matrix = k.repo
          ? await fetchSecretMatrix(k.org, k.repo)
          : await fetchOrgSecretMatrix(k.org);
        return { matrix, partial: false };
      } catch {
        return { matrix: matrixFromNames(await listSecrets(k)), partial: true };
      }
    },
  );
  // `latest` keeps the list on screen while a write refetches it.
  const matrix = () => data.latest?.matrix;
  const envs = () => matrix()?.environments ?? [];
  const picked = () => (envs().includes(props.environment) ? props.environment : "");
  const names = () => (matrix() ? sortedKeys(matrix()!).map((r) => r.key) : []);
  const rowOf = (name: string) => matrix()?.keys.find((r) => r.key === name);

  const [open, setOpen] = createSignal<ReadonlySet<string>>(new Set());
  const [unfolded, setUnfolded] = createSignal<ReadonlySet<string>>(new Set());
  const [editing, setEditing] = createSignal<Cell | null>(null);
  const [clearing, setClearing] = createSignal<Cell | null>(null);
  const [creating, setCreating] = createSignal(false);
  const [silenceFor, setSilenceFor] = createSignal<string | null>(null);
  const silence = useWrite(() => refetch());

  const toggleSilenced = (cell: Cell, on: boolean) => {
    setSilenceFor(cell.key);
    const repo = props.repo;
    if (!repo) return;
    void silence.run(() => setSecretCellSilenced(props.org, repo, cell.column, cell.key, on));
  };

  const where = (column: string) =>
    column === REPO_DEFAULT_COLUMN ? `the ${scopeLabel(column, isOrg())}` : column;

  const ScopeLine = (p: { row: SecretKeyRow; column: string }) => {
    const s = () => statusAt(p.row, p.column);
    const cell = () => ({ key: p.row.key, column: p.column });
    const at = () => `${p.row.key} in ${where(p.column)}`;
    return (
      <li class="skey-line">
        <span class={`skey-g skey-g-${s()}`} aria-hidden="true">{GLYPH[s()]}</span>
        <span class="skey-scope" classList={{ mono: p.column !== REPO_DEFAULT_COLUMN }}>
          {scopeLabel(p.column, isOrg())}
        </span>
        <span class="skey-state">{stateText(p.row, p.column, isOrg())}</span>
        <span class="skey-acts">
          <button
            class="btn btn-ghost btn-sm"
            type="button"
            aria-label={`${s() === "set" ? "Edit" : "Set"} ${at()}`}
            onClick={() => setEditing(cell())}
          >
            {s() === "set" ? "edit" : "set"}
          </button>
          <Show when={s() === "set"}>
            <button
              class="btn btn-ghost btn-sm"
              type="button"
              aria-label={`Clear ${at()}`}
              onClick={() => setClearing(cell())}
            >
              clear
            </button>
          </Show>
          <Show when={s() === "unset" && !isOrg()}>
            <button
              class="btn btn-ghost btn-sm"
              type="button"
              title="Mark this gap as meant to be there. Advisory: it changes nothing a Run gets."
              aria-label={`Mark ${at()} as unset on purpose`}
              disabled={silence.busy()}
              onClick={() => toggleSilenced(cell(), true)}
            >
              unset on purpose
            </button>
          </Show>
          <Show when={s() === "silenced" && !isOrg()}>
            <button
              class="btn btn-ghost btn-sm"
              type="button"
              aria-label={`Unmark ${at()} as unset on purpose`}
              disabled={silence.busy()}
              onClick={() => toggleSilenced(cell(), false)}
            >
              unmark
            </button>
          </Show>
        </span>
      </li>
    );
  };

  const KeyItem = (p: { name: string }) => {
    const id = createUniqueId();
    const row = () => rowOf(p.name);
    const isOpen = () => open().has(p.name);
    const isUnfolded = () => unfolded().has(p.name);
    const lines = () => scopeLines(matrix()!, row()!, picked() || undefined);
    const visible = () =>
      isUnfolded() ? scopeColumns(matrix()!) : lines().shown;
    return (
      <Show when={row()}>
        {(r) => (
          <li class="skey">
            <button
              class="skey-h"
              type="button"
              aria-expanded={isOpen()}
              aria-controls={id}
              onClick={() => setOpen((s) => toggleIn(s, p.name))}
            >
              <Icon icon={isOpen() ? "chevron-down" : "chevron-right"} size={14} />
              <code class="mono skey-name">{p.name}</code>
              <Show when={!isOrg() && isOrgKey(r())}>
                <span class="skey-tag">org</span>
              </Show>
              <span class="skey-sum">
                <Show when={picked()} fallback={keySummary(matrix()!, r(), isOrg())}>
                  <span class={`skey-g skey-g-${statusAt(r(), picked())}`} aria-hidden="true">
                    {GLYPH[statusAt(r(), picked())]}
                  </span>{" "}
                  <span class="mono">{picked()}</span>: {envSummary(r(), picked(), isOrg())}
                </Show>
              </span>
            </button>
            <Show when={isOpen()}>
              <ul class="skey-lines" id={id}>
                <For each={visible()}>{(c) => <ScopeLine row={r()} column={c} />}</For>
                <Show when={lines().folded.length > 0}>
                  <li class="skey-line skey-fold">
                    <span class="skey-state">
                      {isUnfolded() ? "" : "… "}
                      {foldLabel(r(), lines().folded)}
                    </span>
                    <span class="skey-acts">
                      <button
                        class="btn btn-ghost btn-sm"
                        type="button"
                        aria-expanded={isUnfolded()}
                        onClick={() => setUnfolded((s) => toggleIn(s, p.name))}
                      >
                        {isUnfolded() ? "hide" : "show"}
                      </button>
                    </span>
                  </li>
                </Show>
                <Show when={silence.error() && silenceFor() === p.name}>
                  <li class="error">{silence.error()}</li>
                </Show>
              </ul>
            </Show>
          </li>
        )}
      </Show>
    );
  };

  return (
    <div class="panel">
      <div class="panel-h">
        <span>{isOrg() ? `Org secrets · ${props.org}` : "Secrets"}</span>
        <Show when={names().length > 0}>
          <span class="tok-count">{names().length}</span>
        </Show>
      </div>
      <div class="secrets-body">
        <div class="skey-bar">
          <Show when={envs().length > 0}>
            <SearchSelect
              icon="shield-check"
              clearable
              placeholder="all environments"
              searchPlaceholder="Search environments…"
              value={picked()}
              onChange={props.onEnvironment}
              options={envs().map((e) => ({ value: e, label: e }))}
            />
          </Show>
          <button class="btn btn-primary btn-sm" type="button" onClick={() => setCreating(true)}>
            <Icon icon="plus" size={14} /> New secret
          </button>
        </div>

        <Show when={data.error && !data.latest}>
          <p class="error">
            Could not load {isOrg() ? "the org's" : "this repository's"} secrets.
          </p>
        </Show>
        <Show when={data.latest?.partial}>
          <p class="subtle">
            <small>
              Showing the {scopeLabel(REPO_DEFAULT_COLUMN, isOrg())} only: the per-environment
              view could not be loaded.
            </small>
          </p>
        </Show>

        <Show
          when={matrix()}
          fallback={
            <Show when={!data.error}>
              <p class="empty">loading…</p>
            </Show>
          }
        >
          <Show
            when={names().length > 0}
            fallback={
              <p class="empty">
                No {isOrg() ? "org " : ""}secrets yet. Add one with New secret.
              </p>
            }
          >
            <ul class="skey-list">
              <For each={names()}>{(name) => <KeyItem name={name} />}</For>
            </ul>
          </Show>
          <p class="subtle skey-legend">
            <Show
              when={isOrg()}
              fallback={
                <small>
                  <b>●</b> set here · <b>○</b> inherited · <b>–</b> unset · <b>⊘</b> unset on
                  purpose. Encrypted at rest and never displayed: you can overwrite a value but
                  not read it back. A gap never blocks a deploy.
                </small>
              }
            >
              <small>
                <b>●</b> set here · <b>○</b> inherited · <b>–</b> unset. Encrypted at rest and
                never displayed: you can overwrite a value but not read it back. A repository's
                default wins over the Org default, and a repository's value for one Environment
                wins over the org's value for it.
              </small>
            </Show>
          </p>
        </Show>
      </div>

      <Show when={creating()}>
        <NewSecretDialog
          scope={props.repo ? { org: props.org, repo: props.repo } : { org: props.org }}
          defaultLabel={scopeLabel(REPO_DEFAULT_COLUMN, isOrg())}
          existing={(env) => (matrix() ? namesSetAt(matrix()!, env) : [])}
          environments={envs()}
          environment={picked()}
          onClose={() => setCreating(false)}
          onSaved={() => refetch()}
        />
      </Show>
      <Show when={editing()}>
        {(cell) => (
          <CellEditor
            org={props.org}
            repo={props.repo}
            cell={cell()}
            row={rowOf(cell().key)}
            onClose={() => setEditing(null)}
            onSaved={() => refetch()}
          />
        )}
      </Show>
      <Show when={clearing()}>
        {(cell) => (
          <ClearDialog
            org={props.org}
            repo={props.repo}
            cell={cell()}
            onClose={() => setClearing(null)}
            onSaved={() => refetch()}
          />
        )}
      </Show>
    </div>
  );
}

/** What clearing a scope falls back to: stated, never implied. */
const fallbackOf = (column: string, repo: string | undefined) =>
  repo
    ? column === REPO_DEFAULT_COLUMN
      ? "the org value, if the org has one"
      : `the org value for ${column} if the org has one, else whatever the Repository default resolves to`
    : column === REPO_DEFAULT_COLUMN
      ? "nothing from the org; a repository's own value is not affected"
      : "the Org default, unless the repository sets its own";

const eyebrowOf = (org: string, repo: string | undefined) =>
  `${repo ? `${org}/${repo}` : org} · secrets`;

/**
 * Edit ONE scope of a key. Which actions it offers depends on the scope's
 * state: set or replace a value here, clear the value here (falling back to the
 * broader scope), or toggle the advisory "unset on purpose" marker, which only
 * applies to a scope that resolves to nothing.
 */
function CellEditor(props: {
  org: string;
  repo?: string;
  cell: Cell;
  row: SecretKeyRow | undefined;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [value, setValue] = createSignal("");
  const write = useWrite(() => {
    props.onSaved();
    props.onClose();
  });
  const status = () => (props.row ? statusAt(props.row, props.cell.column) : "unset");
  const inheritedFrom = () => props.row?.inherited_from[props.cell.column];
  const scope = () => columnScope(props.org, props.repo, props.cell.column);

  const save = () =>
    write.run(() => putSecret({ ...scope(), name: props.cell.key, value: value() }));
  const clear = () => write.run(() => deleteSecret(scope(), props.cell.key));
  const toggleSilenced = () => {
    const repo = props.repo;
    if (!repo) return;
    return write.run(() =>
      setSecretCellSilenced(
        props.org,
        repo,
        props.cell.column,
        props.cell.key,
        status() !== "silenced",
      ),
    );
  };

  return (
    <Modal
      eyebrow={eyebrowOf(props.org, props.repo)}
      title={
        <>
          <span class="mono">{props.cell.key}</span> ·{" "}
          {scopeLabel(props.cell.column, !props.repo)}
        </>
      }
      purpose={`Encrypted at rest, never displayed. Clearing here falls back to ${fallbackOf(props.cell.column, props.repo)}.`}
      busy={write.busy()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={write.error()}>
            <ModalMessage>{write.error()}</ModalMessage>
          </Show>
          <Show when={status() === "set"}>
            <button class="btn btn-danger" type="button" disabled={write.busy()} onClick={clear}>
              Clear here
            </button>
            <span class="sp" />
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={write.busy()}>
            Cancel
          </button>
          <button
            class="btn btn-primary"
            type="button"
            disabled={write.busy() || !value()}
            onClick={save}
          >
            <Icon icon="key-round" size={14} />{" "}
            {write.busy() ? "Saving…" : status() === "set" ? "Replace" : "Set here"}
          </button>
        </>
      }
    >
      <p class="dlg-text">
        <Show when={status() === "set"}>A value is set at this scope. Saving replaces it.</Show>
        <Show when={status() === "inherited"}>
          Inherited from{" "}
          {!props.repo
            ? "the Org default"
            : inheritedFrom() === "org_environment"
              ? `the org's value for ${props.cell.column}`
              : inheritedFrom() === "org"
                ? "the org"
                : "the Repository default"}{" "}
          today.
          Saving <b>overrides it here only</b>.
        </Show>
        <Show when={status() === "unset" || status() === "silenced"}>
          Nothing resolves here today. Saving sets it at this scope.
        </Show>
      </p>
      <Field label="Value">
        <Input
          mono
          autofocus
          type="password"
          placeholder="value (write-only)"
          value={value()}
          onValue={setValue}
        />
      </Field>
      <Show when={props.repo && (status() === "unset" || status() === "silenced")}>
        <Checkbox
          tone="warn"
          checked={status() === "silenced"}
          disabled={write.busy()}
          onChecked={() => void toggleSilenced()}
          label="Unset on purpose"
          help="Marks the gap as meant to be there. Advisory: it changes nothing a Run gets."
        />
      </Show>
    </Modal>
  );
}

/** Confirm clearing one scope's value; says what the scope falls back to. */
function ClearDialog(props: {
  org: string;
  repo?: string;
  cell: Cell;
  onClose: () => void;
  onSaved: () => void;
}) {
  const write = useWrite(() => {
    props.onSaved();
    props.onClose();
  });
  const clear = () =>
    write.run(() =>
      deleteSecret(columnScope(props.org, props.repo, props.cell.column), props.cell.key),
    );
  return (
    <Modal
      size="narrow"
      eyebrow={eyebrowOf(props.org, props.repo)}
      title={
        <>
          Clear <span class="mono">{props.cell.key}</span> ·{" "}
          {scopeLabel(props.cell.column, !props.repo)}
        </>
      }
      purpose={`The value here is deleted. Runs then get ${fallbackOf(props.cell.column, props.repo)}.`}
      busy={write.busy()}
      onClose={props.onClose}
      footer={
        <>
          <Show when={write.error()}>
            <ModalMessage>{write.error()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={write.busy()}>
            Cancel
          </button>
          <button class="btn btn-danger" type="button" disabled={write.busy()} onClick={clear}>
            {write.busy() ? "Clearing…" : "Clear here"}
          </button>
        </>
      }
    >
      <p class="dlg-text">
        Values are never displayed, so a cleared value cannot be read back. Setting it again needs
        the value.
      </p>
    </Modal>
  );
}
