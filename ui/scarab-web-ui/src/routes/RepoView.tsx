// Repo dashboard (tier 2) — tabs over one repository: Runs (default), Branches,
// Pull requests, Environments, Secrets, Settings. The runs table shows real
// backend runs enriched with representative provenance (commit/branch/trigger/
// author/duration) until the forge slice lands; clicking a run opens the real,
// live run detail. "Run pipeline" opens a manual-trigger dialog with typed
// inputs and creates a run.
import { createResource, createSignal, For, Show } from "solid-js";
import { useParams, useNavigate } from "@solidjs/router";
import {
  listRuns,
  createRun,
  listSecrets,
  putSecret,
  deleteSecret,
  fetchSecretMatrix,
  type SecretCellStatus,
} from "../api/client";
import {
  enrichProvenance,
  environments,
  manualInputs,
  TRIGGER_GLYPH,
  type Provenance,
} from "../data/catalog";
import { relTime, absTime } from "../fmt";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";

type Row = { id: string; status: string; created_at: number; prov: Provenance };
type Tab = "runs" | "branches" | "prs" | "environments" | "secrets" | "settings";

export default function RepoView() {
  const params = useParams();
  const nav = useNavigate();
  const org = () => params.org!;
  const repo = () => params.repo!;

  const [rows, { refetch }] = createResource(
    () => repo(),
    async (r): Promise<Row[]> => {
      const runs = await listRuns(50);
      return runs.map((x) => ({ ...x, prov: enrichProvenance(x.id, r) }));
    },
  );

  const [tab, setTab] = createSignal<Tab>("runs");
  const [statusFilter, setStatusFilter] = createSignal<"all" | "running" | "failed">("all");
  const [showDialog, setShowDialog] = createSignal(false);

  const filtered = () => {
    const all = rows() ?? [];
    const f = statusFilter();
    return f === "all" ? all : all.filter((r) => r.status === f);
  };

  const branches = () => [...new Set((rows() ?? []).map((r) => r.prov.branch))];

  return (
    <section class="page">
      <Doodle icon="workflow" size={230} rotate={12} opacity={0.05} bottom="40px" right="60px" />

      <div class="page-head">
        <h1>{repo()}</h1>
        <button class="btn btn-primary" onClick={() => setShowDialog(true)}>
          <Icon icon="play" size={14} /> Run pipeline
        </button>
      </div>

      <div class="tabs">
        <For each={[
          ["runs", "Runs"],
          ["branches", "Branches"],
          ["prs", "Pull requests"],
          ["environments", "Environments"],
          ["secrets", "Secrets"],
          ["settings", "Settings"],
        ] as [Tab, string][]}>
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
          <Show when={filtered().length > 0} fallback={<p class="empty">No runs match.</p>}>
            <div class="runlist">
              <div class="runrow head">
                <span></span><span>commit</span><span>trigger · branch</span><span>duration</span><span>when</span>
              </div>
              <For each={filtered()}>
                {(r) => (
                  <div class="runrow" onClick={() => nav(`/${org()}/${repo()}/runs/${r.id}`)}>
                    <span class={`sdot ${r.status}`} />
                    <span class="rr-commit">
                      <span class="rr-sha mono">{r.prov.sha}</span>
                      <span class="rr-msg">{r.prov.message}</span>
                    </span>
                    <span class="rr-trigger mono">
                      <span class="tglyph">{TRIGGER_GLYPH[r.prov.trigger]}</span>
                      {r.prov.trigger === "pull_request"
                        ? `PR #${r.prov.prNumber}`
                        : r.prov.trigger === "tag"
                          ? r.prov.tag
                          : r.prov.trigger}
                      <span class="rr-branch"> · {r.prov.branch}</span>
                    </span>
                    <span class="rr-dur mono">{r.status === "pending" ? "—" : r.prov.duration}</span>
                    <span class="rr-when mono" title={absTime(r.created_at)}>{relTime(r.created_at)}</span>
                  </div>
                )}
              </For>
            </div>
          </Show>
        </Show>
      </Show>

      {/* ---- Branches ---- */}
      <Show when={tab() === "branches"}>
        <div class="runlist">
          <For each={branches()} fallback={<p class="empty">no branches</p>}>
            {(b) => (
              <div class="runrow" style={{ "grid-template-columns": "16px 1fr auto" }}>
                <Icon icon="git-branch" size={14} />
                <span class="rr-msg mono">{b}</span>
                <span class="rr-when mono">
                  {(rows() ?? []).filter((r) => r.prov.branch === b).length} runs
                </span>
              </div>
            )}
          </For>
        </div>
      </Show>

      {/* ---- Pull requests ---- */}
      <Show when={tab() === "prs"}>
        <div class="runlist">
          <For
            each={(rows() ?? []).filter((r) => r.prov.trigger === "pull_request")}
            fallback={<p class="empty">no open pull requests</p>}
          >
            {(r) => (
              <div class="runrow" onClick={() => nav(`/${org()}/${repo()}/runs/${r.id}`)}
                style={{ "grid-template-columns": "16px 1fr auto" }}>
                <span class={`sdot ${r.status}`} />
                <span class="rr-msg">PR #{r.prov.prNumber} · {r.prov.message}</span>
                <span class="rr-when mono">{r.prov.branch}</span>
              </div>
            )}
          </For>
        </div>
      </Show>

      {/* ---- Environments ---- */}
      <Show when={tab() === "environments"}>
        <For each={environments(repo())}>
          {(env) => (
            <div class="panel env-panel">
              <div class="panel-h">
                <span>{env.name}</span>
                <span class="subtle mono">current {env.current}</span>
              </div>
              <div class="env-body">
                <div class="env-rules mono">
                  <Icon icon="shield-check" size={13} /> {env.approvers} approvers
                  <span class="dotsep">·</span> {env.wait} wait
                  <span class="dotsep">·</span> {env.allowed}
                </div>
                <div class="env-history">
                  <For each={env.history}>
                    {(h) => (
                      <div class="env-h-row mono">
                        <span class="subtle">{h.when}</span> {h.version}
                        <span class="ok-mark">✓</span> approved {h.by}
                      </div>
                    )}
                  </For>
                </div>
              </div>
            </div>
          )}
        </For>
      </Show>

      {/* ---- Secrets ---- */}
      <Show when={tab() === "secrets"}>
        <RepoSecrets org={org()} repo={repo()} />
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

      <Show when={showDialog()}>
        <RunDialog
          repo={repo()}
          onClose={() => setShowDialog(false)}
          onRun={async () => {
            const id = await createRun({
              pipeline: {
                ir_version: 1,
                steps: [
                  { id: "build", image: "x", command: ["sh", "-c", "echo building; sleep 1"] },
                  { id: "test", image: "x", command: ["sh", "-c", "echo testing; sleep 1"], needs: ["build"] },
                ],
              },
            });
            setShowDialog(false);
            nav(`/${org()}/${repo()}/runs/${id}`);
          }}
        />
      </Show>
    </section>
  );
}

// ---- manual-run dialog ----------------------------------------------------

function RunDialog(props: { repo: string; onClose: () => void; onRun: () => Promise<void> }) {
  const inputs = manualInputs(props.repo);
  const [busy, setBusy] = createSignal(false);
  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h"><span>Run pipeline</span></div>
        <div class="modal-body">
          <div class="form-r">
            <label>branch / ref</label>
            <div class="input select-like">main <Icon icon="chevron-down" size={13} /></div>
          </div>
          <div class="form-r">
            <label>pipeline</label>
            <div class="input select-like">.scarab/ci.yaml <Icon icon="chevron-down" size={13} /></div>
          </div>
          <For each={inputs}>
            {(inp) => (
              <div class="form-r">
                <label>{inp.key} · {inp.type}</label>
                <Show
                  when={inp.type === "string"}
                  fallback={
                    <div class="input select-like">
                      {String((inp as { default: unknown }).default)} <Icon icon="chevron-down" size={13} />
                    </div>
                  }
                >
                  <input class="input" placeholder={inp.key} />
                </Show>
              </div>
            )}
          </For>
          <div class="modal-actions">
            <button
              class="btn btn-primary"
              disabled={busy()}
              onClick={async () => {
                setBusy(true);
                try {
                  await props.onRun();
                } finally {
                  setBusy(false);
                }
              }}
            >
              <Icon icon="play" size={14} /> {busy() ? "starting…" : "Run"}
            </button>
            <button class="btn btn-ghost" onClick={props.onClose}>Cancel</button>
          </div>
          <p class="subtle modal-note">
            Runs a representative pipeline against the local executor. Typed inputs bind at
            trigger time once the inputs backend lands.
          </p>
        </div>
      </div>
    </div>
  );
}

// ---- repo-scoped secrets (real API) --------------------------------------

function RepoSecrets(props: { org: string; repo: string }) {
  const scope = () => ({ org: props.org, repo: props.repo });
  const [names, { refetch }] = createResource(scope, listSecrets);
  const [name, setName] = createSignal("");
  const [value, setValue] = createSignal("");

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
            <input class="input" placeholder="NAME" value={name()} onInput={(e) => setName(e.currentTarget.value)} />
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
// Each key's *effective* status per environment after inheritance. A shared key
// defined once at repo/org scope reads as `inherited` everywhere (never
// missing); a per-env key is `set` where defined and `unset` elsewhere. Purely
// advisory — it never blocks a deploy, and never shows a value.

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
