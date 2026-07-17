// Repo dashboard (tier 2) — tabs over one repository: Runs (default),
// Environments, Secrets, Settings. Branches and pull requests aren't their own
// tabs anymore — they're just ways of slicing runs, so they live as a branch
// filter and a trigger filter on the runs list. The header CTA is contextual to
// the active tab (run a pipeline, add an environment, add a secret).
import { createResource, createSignal, createEffect, For, Show } from "solid-js";
import { useParams, useNavigate } from "@solidjs/router";
import {
  listRuns,
  listSecrets,
  putSecret,
  deleteSecret,
  fetchSecretMatrix,
  listEnvironments,
  listDeployments,
  type RunSummary,
  type RepoEnvironment,
  type SecretCellStatus,
} from "../api/client";
import { relTime, absTime } from "../fmt";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";

type Tab = "runs" | "environments" | "secrets" | "settings";

const TABS: [Tab, string][] = [
  ["runs", "Runs"],
  ["environments", "Environments"],
  ["secrets", "Secrets"],
  ["settings", "Settings"],
];

export default function RepoView() {
  const params = useParams();
  const nav = useNavigate();
  const org = () => params.org!;
  const repo = () => params.repo!;

  // Only this repo's tenanted runs (run.org/project stamped at creation,
  // ADR-0049). Untenanted dev runs (inline `POST /v1/runs`) don't belong to
  // any repo, so they never show here — the dashboard's activity feed has them.
  const [rows, { refetch }] = createResource(
    () => ({ org: org(), repo: repo() }),
    async (k): Promise<RunSummary[]> => {
      const runs = await listRuns(50);
      return runs.filter((r) => r.org === k.org && r.project === k.repo);
    },
  );

  const [tab, setTab] = createSignal<Tab>("runs");
  const [statusFilter, setStatusFilter] = createSignal<"all" | "running" | "failed">("all");
  const [showEnvDialog, setShowEnvDialog] = createSignal(false);
  const [secretFocus, setSecretFocus] = createSignal(0);

  const filtered = () => {
    const all = rows() ?? [];
    const s = statusFilter();
    return s === "all" ? all : all.filter((r) => r.status === s);
  };

  // The header CTA follows the tab: what you'd create in this context.
  const cta = () => {
    switch (tab()) {
      case "runs":
        return {
          label: "Run pipeline",
          icon: "play",
          onClick: () => nav(`/${org()}/${repo()}/run`),
        };
      case "environments":
        return { label: "New environment", icon: "plus", onClick: () => setShowEnvDialog(true) };
      case "secrets":
        return { label: "New secret", icon: "plus", onClick: () => setSecretFocus((n) => n + 1) };
      default:
        return null;
    }
  };

  return (
    <section class="page">
      <Doodle icon="workflow" size={230} rotate={12} opacity={0.16} top="52px" right="48px" />

      <div class="page-head">
        <h1>
          <span class="head-org">{org()}/</span>{repo()}
        </h1>
      </div>
      {/* Always rendered (fixed height) so the tab row never shifts between
          tabs with and without a CTA. */}
      <div class="page-toolbar">
        <Show when={cta()}>
          {(c) => (
            <button class="btn btn-primary" onClick={() => c().onClick()}>
              <Icon icon={c().icon} size={14} /> {c().label}
            </button>
          )}
        </Show>
      </div>

      <div class="tabs">
        <For each={TABS}>
          {([key, label]) => (
            <button class={`tab ${tab() === key ? "on" : ""}`} onClick={() => setTab(key)}>
              {label}
            </button>
          )}
        </For>
      </div>

      {/* ---- Runs ---- */}
      <Show when={tab() === "runs"}>
        <div class="filters">
          <For each={["all", "running", "failed"] as const}>
            {(f) => (
              <button
                class={`fpill ${statusFilter() === f ? "on" : ""}`}
                onClick={() => setStatusFilter(f)}
              >
                {f}
              </button>
            )}
          </For>
          <button class="btn btn-ghost btn-sm filters-refresh" onClick={() => refetch()}>
            <Icon icon="rotate-cw" size={13} /> Refresh
          </button>
        </div>

        <Show when={!rows.loading} fallback={<p class="empty">loading…</p>}>
          <Show when={!rows.error} fallback={<p class="error">Could not load runs.</p>}>
            <Show when={filtered().length > 0} fallback={<p class="empty">No runs match these filters.</p>}>
              <div class="runlist">
                <div class="runrow head">
                  <span></span><span>run</span><span>status</span><span></span><span>when</span>
                </div>
                <For each={filtered()}>
                  {(r) => (
                    <div class="runrow" onClick={() => nav(`/${org()}/${repo()}/runs/${r.id}`)}>
                      <span class={`sdot ${r.status}`} />
                      <span class="rr-commit">
                        <span class="rr-sha mono">{r.id.slice(0, 8)}</span>
                      </span>
                      <span class="rr-trigger mono">{r.status}</span>
                      <span class="rr-dur mono"></span>
                      <span class="rr-when mono" title={absTime(r.created_at)}>{relTime(r.created_at)}</span>
                    </div>
                  )}
                </For>
              </div>
            </Show>
          </Show>
        </Show>
      </Show>

      {/* ---- Environments ---- */}
      <Show when={tab() === "environments"}>
        <RepoEnvironments org={org()} repo={repo()} />
      </Show>

      {/* ---- Secrets ---- */}
      <Show when={tab() === "secrets"}>
        <RepoSecrets org={org()} repo={repo()} focusPing={secretFocus()} />
        <SecretMatrix org={org()} repo={repo()} />
      </Show>

      {/* ---- Settings ---- */}
      <Show when={tab() === "settings"}>
        <div class="panel">
          <div class="panel-h"><span>Settings</span></div>
          <div class="settings-body">
            <div class="set-row"><span class="k">Default branch</span><span class="mono">main</span></div>
            <div class="set-row"><span class="k">Triggers</span><span class="mono">push · pull_request · tag · manual</span></div>
            <div class="set-row"><span class="k">Webhook</span><span class="subtle">connects with the forge integration</span></div>
            <button class="btn btn-danger btn-sm" style={{ "margin-top": "8px" }}>Disable repository</button>
          </div>
        </div>
      </Show>

      <Show when={showEnvDialog()}>
        <EnvDialog repo={repo()} onClose={() => setShowEnvDialog(false)} />
      </Show>
    </section>
  );
}

// ---- environments (real API: `GET …/environments` + per-env deployments) --

function RepoEnvironments(props: { org: string; repo: string }) {
  const key = () => ({ org: props.org, repo: props.repo });
  const [envs] = createResource(key, ({ org, repo }) => listEnvironments(org, repo));

  return (
    <Show when={!envs.loading} fallback={<p class="empty">loading…</p>}>
      <Show when={!envs.error} fallback={<p class="error">Could not load environments.</p>}>
        <Show when={(envs()?.length ?? 0) > 0} fallback={<p class="empty">No environments.</p>}>
          <For each={envs()}>
            {(env) => <EnvPanel org={props.org} repo={props.repo} env={env} />}
          </For>
        </Show>
      </Show>
    </Show>
  );
}

function EnvPanel(props: { org: string; repo: string; env: RepoEnvironment }) {
  const key = () => ({ org: props.org, repo: props.repo, name: props.env.name });
  const [history] = createResource(key, ({ org, repo, name }) =>
    listDeployments(org, repo, name).catch(() => []),
  );
  const rules = () => props.env.protection;
  const allowed = () => (rules().allowed_refs.length ? rules().allowed_refs.join(", ") : "any ref");

  return (
    <div class="panel env-panel">
      <div class="panel-h">
        <span>{props.env.name}</span>
        <Show when={(history() ?? []).length > 0}>
          <span class="subtle mono">current {history()![0]!.git_ref}</span>
        </Show>
      </div>
      <div class="env-body">
        <div class="env-rules mono">
          <Icon icon="shield-check" size={13} /> {rules().approvers.length} approver
          {rules().approvers.length === 1 ? "" : "s"}
          <span class="dotsep">·</span> {rules().wait_timer}s wait
          <span class="dotsep">·</span> {allowed()}
        </div>
        <div class="env-history">
          <For
            each={history() ?? []}
            fallback={<div class="env-h-row mono subtle">no deployments yet</div>}
          >
            {(h) => (
              <div class="env-h-row mono">
                <span class="subtle" title={absTime(h.at)}>{relTime(h.at)}</span> {h.git_ref}
                <Show when={h.approved_by.length > 0}>
                  <span class="ok-mark">✓</span> approved {h.approved_by.join(", ")}
                </Show>
              </div>
            )}
          </For>
        </div>
      </div>
    </div>
  );
}

// ---- new-environment dialog (representative) ------------------------------
// Mirrors the run dialog's honesty: the fields are real (name, gate policy,
// allowed refs) but binding lands with the environments backend.

function EnvDialog(props: { repo: string; onClose: () => void }) {
  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h"><span>New environment</span></div>
        <div class="modal-body">
          <div class="form-r">
            <label>name</label>
            <input class="input" placeholder="e.g. production" />
          </div>
          <div class="form-r">
            <label>required approvers</label>
            <div class="input select-like">1 <Icon icon="chevron-down" size={13} /></div>
          </div>
          <div class="form-r">
            <label>allowed refs</label>
            <div class="input select-like">main only <Icon icon="chevron-down" size={13} /></div>
          </div>
          <div class="modal-actions">
            <button class="btn btn-primary" onClick={props.onClose}>
              <Icon icon="plus" size={14} /> Create
            </button>
            <button class="btn btn-ghost" onClick={props.onClose}>Cancel</button>
          </div>
          <p class="subtle modal-note">
            Defines a deploy gate (approvers, wait, allowed refs). Persists once the
            environments backend lands (ADR-0037).
          </p>
        </div>
      </div>
    </div>
  );
}

// ---- repo-scoped secrets (real API) --------------------------------------

function RepoSecrets(props: { org: string; repo: string; focusPing: number }) {
  const scope = () => ({ org: props.org, repo: props.repo });
  const [names, { refetch }] = createResource(scope, listSecrets);
  const [name, setName] = createSignal("");
  const [value, setValue] = createSignal("");
  let nameRef: HTMLInputElement | undefined;

  // The header "New secret" CTA pings us to focus the add-secret form. Guard on
  // > 0 so we don't grab focus/scroll on the initial mount.
  createEffect(() => {
    if (props.focusPing > 0 && nameRef) {
      nameRef.scrollIntoView({ block: "center", behavior: "smooth" });
      nameRef.focus();
    }
  });

  return (
    <div class="panel">
      <div class="panel-h"><span>Secrets · {props.org}/{props.repo}</span></div>
      <div class="secrets-body">
        <Show when={!names.loading} fallback={<p class="empty">loading…</p>}>
          <Show when={(names()?.length ?? 0) > 0} fallback={<p class="empty">No secrets at this scope.</p>}>
            <ul class="secret-list">
              <For each={names()}>
                {(n) => (
                  <li class="secret-row">
                    <Icon icon="key-round" size={15} />
                    <code class="mono">{n}</code>
                    <button class="btn btn-danger btn-sm" onClick={async () => { await deleteSecret(scope(), n); refetch(); }}>
                      delete
                    </button>
                  </li>
                )}
              </For>
            </ul>
          </Show>
          <form
            class="form-row secrets-add"
            onSubmit={async (e) => {
              e.preventDefault();
              await putSecret({ ...scope(), name: name().trim(), value: value() });
              setName(""); setValue(""); refetch();
            }}
          >
            <input ref={nameRef} class="input" placeholder="NAME" value={name()} onInput={(e) => setName(e.currentTarget.value)} />
            <input class="input" type="password" placeholder="value (write-only)" value={value()} onInput={(e) => setValue(e.currentTarget.value)} />
            <button class="btn btn-primary" type="submit" disabled={!name().trim()}>Save</button>
          </form>
          <p class="subtle"><small>Encrypted at rest, never displayed — overwrite but never read back.</small></p>
        </Show>
      </div>
    </div>
  );
}

// ---- secret parity matrix (ADR-0037, advisory) ---------------------------

const CELL: Record<SecretCellStatus, { glyph: string; label: string }> = {
  set: { glyph: "●", label: "set here" },
  inherited: { glyph: "○", label: "inherited" },
  unset: { glyph: "–", label: "unset" },
};

function SecretMatrix(props: { org: string; repo: string }) {
  const key = () => ({ org: props.org, repo: props.repo });
  const [matrix] = createResource(key, ({ org, repo }) => fetchSecretMatrix(org, repo));

  return (
    <div class="panel">
      <div class="panel-h"><span>Secret coverage</span></div>
      <div class="secrets-body">
        <Show when={!matrix.loading} fallback={<p class="empty">loading…</p>}>
          <Show
            when={(matrix()?.keys.length ?? 0) > 0}
            fallback={<p class="empty">No environment secrets to compare.</p>}
          >
            <div class="matrix-scroll">
              <table class="secret-matrix">
                <thead>
                  <tr>
                    <th class="mkey">key</th>
                    <For each={matrix()!.environments}>{(e) => <th>{e}</th>}</For>
                  </tr>
                </thead>
                <tbody>
                  <For each={matrix()!.keys}>
                    {(row) => (
                      <tr>
                        <td class="mkey"><code class="mono">{row.key}</code></td>
                        <For each={matrix()!.environments}>
                          {(e) => {
                            const s = (row.status[e] ?? "unset") as SecretCellStatus;
                            return (
                              <td class={`mcell m-${s}`} title={CELL[s].label}>
                                {CELL[s].glyph}
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
                Advisory, after inheritance: <b>●</b> set here · <b>○</b> inherited from
                repo/org · <b>–</b> unset. A gap may be intentional — deploys aren't blocked.
              </small>
            </p>
          </Show>
        </Show>
      </div>
    </div>
  );
}
