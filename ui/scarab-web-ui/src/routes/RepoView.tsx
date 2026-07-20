// Repo dashboard (tier 2) — tabs over one repository: Runs (default),
// Environments, Secrets, Settings. Each run row shows its origin as a grid of
// facts (author · ref · duration) under a header line (sha · status · trigger ·
// when), fed by the origin fields RunSummaryDto now carries. Duration ticks live
// for in-flight runs. The runs list carries a status filter; branch/trigger/PR
// *filters* (removed in the ADR-0054 de-mock) can now be restored on top of
// these fields. The header CTA is contextual to the active tab.
import { createResource, createSignal, createEffect, onCleanup, For, Show } from "solid-js";
import { useParams, useNavigate } from "@solidjs/router";
import { recordVisit } from "../visited";
import {
  listRepoRuns,
  repoForgeUrl,
  listSecrets,
  putSecret,
  deleteSecret,
  fetchSecretMatrix,
  listEnvironments,
  listDeployments,
  putEnvironment,
  type RunSummary,
  type RepoEnvironment,
  type SecretCellStatus,
  type SecretScope,
} from "../api/client";
import { relTime, absTime, duration } from "../fmt";
import { forgeCommitUrl, forgePrUrl } from "../forge";
import TriggerCell from "../components/TriggerCell";
import RunNumber from "../components/RunNumber";
import Icon from "../components/Icon";
import SearchSelect from "../components/SearchSelect";
import Doodle from "../components/Doodle";
import AsciiScene from "../components/AsciiScene";
import ponderIdle from "../../../brand/ascii/generated/ponder-ponder.json";

type Tab = "runs" | "environments" | "secrets" | "settings";

const TABS: [Tab, string][] = [
  ["runs", "Runs"],
  ["environments", "Environments"],
  ["secrets", "Secrets"],
  ["settings", "Settings"],
];

// A run is "live" (still accruing wall time) until it reaches a terminal status.
const TERMINAL = new Set(["succeeded", "failed", "cancelled", "dead_lettered"]);
const isLive = (status: string) => !TERMINAL.has(status);

// The human ref a run ran on: a PR number wins (it's the clearer identity), else
// the branch/tag with its `refs/{heads,tags}/` prefix stripped. `null` when the
// run carries no ref (e.g. a cron trigger).
function refLabel(r: RunSummary): string | null {
  if (r.pr_number != null) return `PR #${r.pr_number}`;
  const ref = r.git_ref;
  if (!ref) return null;
  return ref.replace(/^refs\/heads\//, "").replace(/^refs\/tags\//, "");
}

// The trigger kind as a short display token (`pull_request` → `PR`); the raw
// TriggerKind vocabulary otherwise.
function triggerLabel(kind: string | null | undefined): string | null {
  if (!kind) return null;
  return kind === "pull_request" ? "PR" : kind;
}

// The branch/tag a run ran on: `git_ref` with its `refs/{heads,tags}/` prefix
// stripped. Unlike `refLabel`, this ignores `pr_number` — a PR run still ran on
// a head branch, and the branch filter should match it. `null` when refless.
function branchLabel(r: RunSummary): string | null {
  const ref = r.git_ref;
  if (!ref) return null;
  return ref.replace(/^refs\/heads\//, "").replace(/^refs\/tags\//, "");
}

// Distinct, non-empty values a projection takes across the fetched rows — the
// option set for a filter dropdown. Sorted by the caller.
function distinct<T>(rows: RunSummary[], pick: (r: RunSummary) => T | null | undefined): T[] {
  const seen = new Set<T>();
  for (const r of rows) {
    const v = pick(r);
    if (v != null && v !== "") seen.add(v);
  }
  return [...seen];
}

// Duration to show: a terminal run's total wall time (frozen `duration_ms`), or
// a live run's elapsed since creation against the ticking `now`.
function durationLabel(r: RunSummary, now: number): string {
  return isLive(r.status)
    ? duration(r.created_at, now)
    : duration(0, r.duration_ms);
}

export default function RepoView() {
  const params = useParams();
  const nav = useNavigate();
  const org = () => params.org!;
  const repo = () => params.repo!;

  // Remember this repo for the dashboard's "recently visited" row (browser-local).
  createEffect(() => recordVisit(org(), repo()));

  // Only this repo's tenanted runs, straight from the per-repo endpoint (ADR-0046).
  const [rows, { refetch }] = createResource(
    () => ({ org: org(), repo: repo() }),
    (k): Promise<RunSummary[]> => listRepoRuns(k.org, k.repo, 50),
  );
  // The repo's forge web base (for commit/PR deep links); shared by all rows.
  const [repoUrl] = createResource(
    () => ({ org: org(), repo: repo() }),
    (k) => repoForgeUrl(k.org, k.repo).catch(() => null),
  );

  const [tab, setTab] = createSignal<Tab>("runs");
  const [statusFilter, setStatusFilter] = createSignal<"all" | "running" | "failed">("all");
  // Origin filters, client-side over the already-fetched rows. `""` = "any";
  // `prFilter` holds the PR number stringified (`<select>` values are strings).
  const [authorFilter, setAuthorFilter] = createSignal("");
  const [branchFilter, setBranchFilter] = createSignal("");
  const [prFilter, setPrFilter] = createSignal("");
  const [showEnvDialog, setShowEnvDialog] = createSignal(false);
  const [envEpoch, setEnvEpoch] = createSignal(0); // bump to refetch the env list after a create
  const [secretFocus, setSecretFocus] = createSignal(0);

  // A 1s heartbeat so in-flight runs show a live, ticking elapsed time. Terminal
  // runs use their frozen `duration_ms` and ignore this.
  const [now, setNow] = createSignal(Date.now());
  const tick = setInterval(() => setNow(Date.now()), 1000);
  onCleanup(() => clearInterval(tick));

  // Option sets for the origin dropdowns, derived from whatever the fetched rows
  // actually carry — so you only ever pick a value that exists. A dropdown with
  // no values is hidden (see the JSX), not shown empty.
  const authors = () => distinct(rows() ?? [], (r) => r.actor).sort();
  const branches = () => distinct(rows() ?? [], branchLabel).sort();
  const prNumbers = () => distinct(rows() ?? [], (r) => r.pr_number).sort((a, b) => b - a);

  const anyFilterOn = () =>
    statusFilter() !== "all" || !!authorFilter() || !!branchFilter() || !!prFilter();

  const clearFilters = () => {
    setStatusFilter("all");
    setAuthorFilter("");
    setBranchFilter("");
    setPrFilter("");
  };

  // All predicates AND together. Each origin field is independently nullable, so
  // an engaged filter simply excludes any row that can't match it (older runs
  // predating origin-stamping fall out here) — but a disengaged filter (`""` /
  // `"all"`) never hides a row for lacking that field.
  const filtered = () => {
    const s = statusFilter();
    const author = authorFilter();
    const branch = branchFilter();
    const pr = prFilter();
    return (rows() ?? []).filter((r) => {
      if (s !== "all" && r.status !== s) return false;
      if (author && r.actor !== author) return false;
      if (branch && branchLabel(r) !== branch) return false;
      if (pr && String(r.pr_number ?? "") !== pr) return false;
      return true;
    });
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

          <Show when={authors().length > 0}>
            <SearchSelect
              icon="user"
              clearable
              placeholder="any author"
              searchPlaceholder="Search authors…"
              value={authorFilter()}
              onChange={setAuthorFilter}
              options={authors().map((a) => ({ value: a, label: a }))}
            />
          </Show>

          <Show when={branches().length > 0}>
            <SearchSelect
              icon="git-branch"
              clearable
              placeholder="any branch"
              searchPlaceholder="Search branches…"
              value={branchFilter()}
              onChange={setBranchFilter}
              options={branches().map((b) => ({ value: b, label: b }))}
            />
          </Show>

          <Show when={prNumbers().length > 0}>
            <SearchSelect
              icon="git-pull-request"
              clearable
              placeholder="any PR"
              searchPlaceholder="Search PRs…"
              value={prFilter()}
              onChange={setPrFilter}
              options={prNumbers().map((p) => ({ value: String(p), label: `PR #${p}` }))}
            />
          </Show>

          <Show when={anyFilterOn()}>
            <button class="fpill" onClick={clearFilters}>clear</button>
          </Show>

          <button class="btn btn-ghost btn-sm filters-refresh" onClick={() => refetch()}>
            <Icon icon="rotate-cw" size={13} /> Refresh
          </button>
        </div>

        <Show when={!rows.loading} fallback={<p class="empty">loading…</p>}>
          <Show when={!rows.error} fallback={<p class="error">Could not load runs.</p>}>
            <Show
              when={filtered().length > 0}
              fallback={
                (rows() ?? []).length === 0 ? (
                  <div class="empty-scene">
                    <AsciiScene
                      scene={ponderIdle}
                      fontSize={5}
                      label="Beetle pausing beside its ball"
                      line={"no runs yet.\npush something\nto get rolling."}
                    />
                    <p class="empty">This repo hasn't run yet.</p>
                  </div>
                ) : (
                  <p class="empty">No runs match these filters.</p>
                )
              }
            >
              <div class="runlist">
                <For each={filtered()}>
                  {(r) => {
                    const commitUrl = () => forgeCommitUrl(repoUrl(), r.sha);
                    const prUrl = () => forgePrUrl(repoUrl(), r.pr_number);
                    const stop = (e: MouseEvent) => e.stopPropagation();
                    return (
                      <div class="runrow" onClick={() => nav(`/${org()}/${repo()}/runs/${r.id}`)}>
                        {/* Run-number gutter (ADR-0057 A·2): the per-repo `#N`
                            handle, sequential so it scans down the list. */}
                        <div class="rr-gutter">
                          <RunNumber n={r.run_number} id={r.id} />
                        </div>
                        <div class="rr-body">
                          {/* Primary line: the run Headline (trigger kind + title),
                              the SAME shared cell the run-detail bar uses (ADR-0057). */}
                          <div class="rr-head">
                            <TriggerCell kind={r.trigger_kind} title={r.trigger_title} variant="row" />
                          </div>
                          {/* Secondary line: the origin facts — pipeline · commit ·
                              base ← head · PR · who · when. */}
                          <div class="rr-facts">
                            <Show when={r.pipeline}>
                              {(name) => (
                                <span class="pfact strong" title="pipeline">
                                  <Icon icon="workflow" size={12} />
                                  <span class="mono">{name()}</span>
                                </span>
                              )}
                            </Show>
                            <a
                              class="pfact"
                              classList={{ link: !!commitUrl() }}
                              href={commitUrl() ?? undefined}
                              target="_blank"
                              rel="noopener noreferrer"
                              onClick={stop}
                              title="commit on the forge"
                            >
                              <Icon icon="git-commit-horizontal" size={12} />
                              <span class="mono sha">{(r.sha ?? r.id).slice(0, 7)}</span>
                            </a>
                            {/* base ← head on PR runs (ADR-0057); else the branch/ref. */}
                            <Show
                              when={r.origin_pr_base}
                              fallback={
                                <Show when={branchLabel(r) && branchLabel(r) !== r.sha}>
                                  {(b) => (
                                    <span class="pfact" title="branch / ref">
                                      <Icon icon="git-branch" size={12} />
                                      <span class="mono">{b()}</span>
                                    </span>
                                  )}
                                </Show>
                              }
                            >
                              {(base) => (
                                <span class="pfact" title="base ← head">
                                  <Icon icon="git-branch" size={12} />
                                  <span class="mono">{base()}</span>
                                  <span class="pr-arrow">←</span>
                                  <span class="mono sha">{(r.sha ?? "").slice(0, 7)}</span>
                                </span>
                              )}
                            </Show>
                            <Show when={r.pr_number != null}>
                              <a
                                class="pfact"
                                classList={{ link: !!prUrl() }}
                                href={prUrl() ?? undefined}
                                target="_blank"
                                rel="noopener noreferrer"
                                onClick={stop}
                                title="pull request on the forge"
                              >
                                <Icon icon="git-pull-request" size={12} />
                                <span class="mono">#{r.pr_number}</span>
                              </a>
                            </Show>
                            <Show when={r.actor}>
                              {(actor) => (
                                <span class="pfact" title="triggered by">
                                  <Icon icon="user" size={12} />
                                  <span>{actor()}</span>
                                </span>
                              )}
                            </Show>
                            <span
                              class="pfact"
                              classList={{ live: isLive(r.status) }}
                              title={absTime(r.created_at)}
                            >
                              <Icon icon="timer" size={12} />
                              <span>
                                {relTime(r.created_at)} · <span class="mono">{durationLabel(r, now())}</span>
                              </span>
                            </span>
                          </div>
                        </div>

                        <div class="rr-right">
                          <span class={`rr-status ${r.status}`}>
                            <span class={`sdot ${r.status}`} /> {r.status}
                          </span>
                        </div>
                      </div>
                    );
                  }}
                </For>
              </div>
            </Show>
          </Show>
        </Show>
      </Show>

      {/* ---- Environments ---- */}
      <Show when={tab() === "environments"}>
        <RepoEnvironments org={org()} repo={repo()} epoch={envEpoch()} />
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
        <EnvDialog
          org={org()}
          repo={repo()}
          onClose={() => setShowEnvDialog(false)}
          onCreated={() => setEnvEpoch((n) => n + 1)}
        />
      </Show>
    </section>
  );
}

// ---- environments (real API: `GET …/environments` + per-env deployments) --

function RepoEnvironments(props: { org: string; repo: string; epoch: number }) {
  // `epoch` is in the key so a create in EnvDialog re-fetches the list.
  const key = () => ({ org: props.org, repo: props.repo, epoch: props.epoch });
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
          <Show when={rules().require_reason}>
            <span class="dotsep">·</span> reason required
          </Show>
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

// ---- new-environment dialog ----------------------------------------------
// Wired to `PUT …/environments/{name}` (ADR-0037). Fields map 1:1 onto
// ProtectionRules: comma lists → string[], numbers → wait_timer/concurrency.

const splitList = (s: string): string[] =>
  s.split(",").map((x) => x.trim()).filter((x) => x.length > 0);

function EnvDialog(props: {
  org: string;
  repo: string;
  onClose: () => void;
  onCreated: () => void;
}) {
  const [name, setName] = createSignal("");
  const [approvers, setApprovers] = createSignal("");
  const [waitTimer, setWaitTimer] = createSignal("0");
  const [allowedRefs, setAllowedRefs] = createSignal("main");
  const [concurrency, setConcurrency] = createSignal("1");
  const [requireReason, setRequireReason] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const save = async () => {
    const n = name().trim();
    if (!n) {
      setError("Name is required.");
      return;
    }
    setSaving(true);
    setError(null);
    try {
      await putEnvironment(props.org, props.repo, n, {
        approvers: splitList(approvers()),
        wait_timer: Number(waitTimer()) || 0,
        allowed_refs: splitList(allowedRefs()),
        concurrency: Number(concurrency()) || 1,
        require_reason: requireReason(),
      });
      props.onCreated();
      props.onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save environment.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h"><span>New environment</span></div>
        <div class="modal-body">
          <div class="form-r">
            <label>name</label>
            <input
              class="input"
              placeholder="e.g. production"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
            />
          </div>
          <div class="form-r">
            <label>approvers</label>
            <input
              class="input"
              placeholder="comma-separated users (blank = none)"
              value={approvers()}
              onInput={(e) => setApprovers(e.currentTarget.value)}
            />
          </div>
          <div class="form-r">
            <label>wait timer (s)</label>
            <input
              class="input"
              type="number"
              min="0"
              value={waitTimer()}
              onInput={(e) => setWaitTimer(e.currentTarget.value)}
            />
          </div>
          <div class="form-r">
            <label>allowed refs</label>
            <input
              class="input"
              placeholder="comma-separated (blank = any ref)"
              value={allowedRefs()}
              onInput={(e) => setAllowedRefs(e.currentTarget.value)}
            />
          </div>
          <div class="form-r">
            <label>concurrency</label>
            <input
              class="input"
              type="number"
              min="1"
              value={concurrency()}
              onInput={(e) => setConcurrency(e.currentTarget.value)}
            />
          </div>
          <div class="form-r">
            <label>require reason</label>
            <label class="checkbox-inline">
              <input
                type="checkbox"
                checked={requireReason()}
                onInput={(e) => setRequireReason(e.currentTarget.checked)}
              />
              <span class="subtle">manual/api dispatches must supply a reason</span>
            </label>
          </div>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={saving()} onClick={save}>
              <Icon icon="plus" size={14} /> {saving() ? "Creating…" : "Create"}
            </button>
            <button class="btn btn-ghost" onClick={props.onClose}>Cancel</button>
          </div>
          <p class="subtle modal-note">
            Defines a deploy gate (approvers, wait timer, allowed refs). Requires the
            Administer capability on this repo (ADR-0037).
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
  const [showDialog, setShowDialog] = createSignal(false);

  // The header "New secret" CTA pings us to open the add-secret modal. Guard on
  // > 0 so we don't pop the dialog on the initial mount.
  createEffect(() => {
    if (props.focusPing > 0) setShowDialog(true);
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
function NewSecretDialog(props: {
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
