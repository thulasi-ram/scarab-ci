// Repo dashboard (tier 2) — tabs over one repository: Runs (default),
// Environments, Secrets, Settings. Each run row shows its origin as a grid of
// facts (author · ref · duration) under a header line (sha · status · trigger ·
// when), fed by the origin fields RunSummaryDto now carries. Duration ticks live
// for in-flight runs. The runs list filters by status, author, branch, PR,
// pipeline, environment and deploys — environment and deploys on the server
// (ADR-0074 §2: it is how a Project's older deploys are found), the rest over
// the loaded page. The header CTA is contextual to the active tab.
//
// The active tab and every filter live in the URL query (`?tab=secrets`,
// `?pipeline=ci&env=production`), not in component signals — so a refresh, a
// shared link and the back button all land where you were. The defaults (Runs
// tab, no filter) are the absent params, which keeps the bare repo URL clean.
import { createResource, createSignal, createEffect, onCleanup, For, Show } from "solid-js";
import { useParams, useNavigate, useSearchParams } from "@solidjs/router";
import { recordVisit } from "../visited";
import {
  listRepoRuns,
  repoForgeUrl,
  listEnvironments,
  listApprovalHistory,
  putEnvironmentOverlay,
  deleteEnvironmentOverlay,
  type RunSummary,
  type ProjectEnvironment,
} from "../api/client";
import {
  isPermissive,
  refLayersLabel,
  rulesSummary,
  rulesToForm,
  tightenable,
  toProtectionBody,
} from "../environment-overlays";
import { relTime, absTime, duration } from "../fmt";
import { forgeCommitUrl, forgePrUrl } from "../forge";
import TriggerCell from "../components/TriggerCell";
import RunNumber from "../components/RunNumber";
import Icon from "../components/Icon";
import SearchSelect from "../components/SearchSelect";
import PrincipalSearch from "../components/PrincipalSearch";
import { addToList } from "../identities";
import ScopedSecrets from "../components/ScopedSecrets";
import SecretMatrixEditor from "../components/SecretMatrixEditor";
import Doodle from "../components/Doodle";
import AsciiScene from "../components/AsciiScene";
import { runEnvironmentOptions, runListFilter } from "../runs-filter";
import ponderIdle from "../../../brand/ascii/generated/ponder-ponder.json";

type Tab = "runs" | "environments" | "secrets" | "settings";
type StatusFilter = "all" | "running" | "failed";

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

// One search-param value as a string. The router types a param as possibly
// repeated (`?a=1&a=2`); a hand-edited URL like that just takes the first.
const one = (v: string | string[] | undefined): string =>
  (Array.isArray(v) ? v[0] : v) ?? "";

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

  // The repo's forge web base (for commit/PR deep links); shared by all rows.
  const [repoUrl] = createResource(
    () => ({ org: org(), repo: repo() }),
    (k) => repoForgeUrl(k.org, k.repo).catch(() => null),
  );

  // Tab + filters are URL state (see the header comment). An unknown `?tab=`
  // falls back to Runs rather than rendering an empty page.
  const [search, setSearch] = useSearchParams();
  const tab = (): Tab => {
    const t = one(search.tab);
    return TABS.some(([k]) => k === t) ? (t as Tab) : "runs";
  };
  // A tab switch is a navigation (pushes history, so Back returns to the
  // previous tab); a filter tweak replaces the entry so Back doesn't replay
  // every keystroke of a search.
  const setTab = (t: Tab) => setSearch({ tab: t === "runs" ? "" : t });
  const filterSetter = (key: string) => (v: string) => setSearch({ [key]: v }, { replace: true });
  const statusFilter = (): StatusFilter => {
    const v = one(search.status);
    return v === "running" || v === "failed" ? v : "all";
  };
  const setStatusFilter = (f: StatusFilter) => filterSetter("status")(f === "all" ? "" : f);
  // Origin filters, client-side over the already-fetched rows (the Environment
  // and Deploys filters are sent to the server instead; see `rows`). `""` = "any";
  // `prFilter` holds the PR number stringified (search params are strings).
  const authorFilter = () => one(search.author);
  const setAuthorFilter = filterSetter("author");
  const branchFilter = () => one(search.branch);
  const setBranchFilter = filterSetter("branch");
  const prFilter = () => one(search.pr);
  const setPrFilter = filterSetter("pr");
  const pipelineFilter = () => one(search.pipeline);
  const setPipelineFilter = filterSetter("pipeline");
  const envFilter = () => one(search.env);
  const setEnvFilter = filterSetter("env");
  // Deploys toggle (`?deploy=1`): only deploy Runs — a pipeline or Step marked
  // `deploy: true` (ADR-0074 §2). Server-side, like the Environment.
  const deploysOnly = () => one(search.deploy) === "1";
  const setDeploysOnly = (on: boolean) => filterSetter("deploy")(on ? "1" : "");

  // Only this repo's tenanted runs, straight from the per-repo endpoint
  // (ADR-0046). The Environment and Deploys filters are part of the request,
  // so they fetch the newest matching runs rather than filtering the newest
  // page.
  const [rows, { refetch }] = createResource(
    () => ({ org: org(), repo: repo(), env: envFilter(), deploys: deploysOnly() }),
    (k): Promise<RunSummary[]> =>
      listRepoRuns(k.org, k.repo, 50, runListFilter(k.env, k.deploys)),
  );
  // The Project's Environments, for the Environment picker's options — the
  // loaded rows are already one Environment's once the filter is on. A caller
  // who can't read them still gets the Environments seen on runs.
  const [projectEnvs] = createResource(
    () => ({ org: org(), repo: repo() }),
    (k) =>
      listEnvironments(k.org, k.repo)
        .then((es) => es.map((e) => e.name))
        .catch(() => [] as string[]),
  );

  /** `undefined` = closed; `""` = open with no Environment picked; a name =
   *  open on that Environment (its overlay prefilled). */
  const [envDialog, setEnvDialog] = createSignal<string | undefined>(undefined);
  const [envEpoch, setEnvEpoch] = createSignal(0); // bump to refetch the env list after a create
  const [secretFocus, setSecretFocus] = createSignal(0);
  // The two secret panels read the same store through separate fetches; each
  // bumps the OTHER's epoch after a write so neither shows a stale view.
  const [secretListEpoch, setSecretListEpoch] = createSignal(0);
  const [secretMatrixEpoch, setSecretMatrixEpoch] = createSignal(0);

  // A 1s heartbeat so in-flight runs show a live, ticking elapsed time. Terminal
  // runs use their frozen `duration_ms` and ignore this.
  const [now, setNow] = createSignal(Date.now());
  const tick = setInterval(() => setNow(Date.now()), 1000);
  onCleanup(() => clearInterval(tick));

  // Option sets for the origin dropdowns, derived from whatever the fetched rows
  // actually carry — so you only ever pick a value that exists. A dropdown with
  // no values is hidden (see the JSX), not shown empty. Environments also take
  // the Project's own list, since the rows are filtered on the server.
  const authors = () => distinct(rows() ?? [], (r) => r.actor).sort();
  const branches = () => distinct(rows() ?? [], branchLabel).sort();
  const prNumbers = () => distinct(rows() ?? [], (r) => r.pr_number).sort((a, b) => b - a);
  const pipelines = () => distinct(rows() ?? [], (r) => r.pipeline).sort();
  const environments = () =>
    runEnvironmentOptions(projectEnvs() ?? [], rows() ?? [], envFilter());

  const anyFilterOn = () =>
    statusFilter() !== "all" ||
    !!authorFilter() ||
    !!branchFilter() ||
    !!prFilter() ||
    !!pipelineFilter() ||
    !!envFilter() ||
    deploysOnly();

  const clearFilters = () =>
    setSearch(
      { status: "", author: "", branch: "", pr: "", pipeline: "", env: "", deploy: "" },
      { replace: true },
    );

  // All predicates AND together. Each origin field is independently nullable, so
  // an engaged filter simply excludes any row that can't match it (older runs
  // predating origin-stamping fall out here) — but a disengaged filter (`""` /
  // `"all"`) never hides a row for lacking that field.
  const filtered = () => {
    const s = statusFilter();
    const author = authorFilter();
    const branch = branchFilter();
    const pr = prFilter();
    const pipeline = pipelineFilter();
    const env = envFilter();
    return (rows() ?? []).filter((r) => {
      if (s !== "all" && r.status !== s) return false;
      if (author && r.actor !== author) return false;
      if (branch && branchLabel(r) !== branch) return false;
      if (pr && String(r.pr_number ?? "") !== pr) return false;
      if (pipeline && r.pipeline !== pipeline) return false;
      // Already applied by the server; kept so a page still in flight from
      // the previous filter never shows under the new one.
      if (env && r.environment !== env) return false;
      if (deploysOnly() && !r.deploy) return false;
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
        return {
          label: "Tighten for this project",
          icon: "shield-check",
          onClick: () => setEnvDialog(""),
        };
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
            <button
              class={`tab ${tab() === key ? "on" : ""}`}
              data-label={label}
              onClick={() => setTab(key)}
            >
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

          <button
            class={`fpill ${deploysOnly() ? "on" : ""}`}
            title="only deploy runs: a pipeline or step marked `deploy: true`"
            onClick={() => setDeploysOnly(!deploysOnly())}
          >
            <Icon icon="rocket" size={12} /> deploys
          </button>

          <Show when={pipelines().length > 0}>
            <SearchSelect
              icon="workflow"
              clearable
              placeholder="any pipeline"
              searchPlaceholder="Search pipelines…"
              value={pipelineFilter()}
              onChange={setPipelineFilter}
              options={pipelines().map((p) => ({ value: p, label: p }))}
            />
          </Show>

          {/* Only Environment-governed runs carry an environment (their
              pipeline declared `environment:`), so this appears once the
              Project has an Environment or a run governed by one. */}
          <Show when={environments().length > 0}>
            <SearchSelect
              icon="shield-check"
              clearable
              placeholder="any environment"
              searchPlaceholder="Search environments…"
              value={envFilter()}
              onChange={setEnvFilter}
              options={environments().map((e) => ({ value: e, label: e }))}
            />
          </Show>

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
                (rows() ?? []).length === 0 && !envFilter() ? (
                  <div class="empty-scene">
                    <div class="empty-land">
                      <AsciiScene
                        scene={ponderIdle}
                        fontSize={4}
                        label="Beetle dozing with its back against its dung ball"
                      />
                    </div>
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
                            <TriggerCell kind={r.trigger_kind} title={r.trigger_title} />
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
                            <Show when={r.deploy}>
                              <span
                                class="pfact strong"
                                title="deploy run: its pipeline or a step is marked `deploy: true`"
                              >
                                <Icon icon="rocket" size={12} /> deploy
                              </span>
                            </Show>
                            <Show when={r.environment}>
                              {(env) => (
                                <span class="pfact" title="environment">
                                  <Icon icon="shield-check" size={12} />
                                  <span class="mono">{env()}</span>
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
        <RepoEnvironments
          org={org()}
          repo={repo()}
          epoch={envEpoch()}
          onTighten={(name) => setEnvDialog(name)}
          onChanged={() => setEnvEpoch((n) => n + 1)}
        />
      </Show>

      {/* ---- Secrets ---- */}
      <Show when={tab() === "secrets"}>
        <ScopedSecrets
          scope={{ org: org(), repo: repo() }}
          title={`Secrets · ${org()}/${repo()}`}
          focusPing={secretFocus()}
          epoch={secretListEpoch()}
          onChange={() => setSecretMatrixEpoch((n) => n + 1)}
        />
        <SecretMatrixEditor
          org={org()}
          repo={repo()}
          epoch={secretMatrixEpoch()}
          onChange={() => setSecretListEpoch((n) => n + 1)}
        />
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

      <Show when={envDialog() !== undefined}>
        <EnvDialog
          org={org()}
          repo={repo()}
          initial={envDialog() || undefined}
          onClose={() => setEnvDialog(undefined)}
          onSaved={() => setEnvEpoch((n) => n + 1)}
        />
      </Show>
    </section>
  );
}


// ---- environments (ADR-0071: org Environments as this project sees them) --
// Each panel shows the org's BASELINE, this project's OVERLAY and the
// EFFECTIVE rules — the last computed by the server, never merged here. The
// project can only tighten (its overlay); the baseline, who may deploy and
// any privileges are an org admin's, under Settings › Environments.

function RepoEnvironments(props: {
  org: string;
  repo: string;
  epoch: number;
  onTighten: (env: string) => void;
  onChanged: () => void;
}) {
  // `epoch` is in the key so a save in EnvDialog re-fetches the list.
  const key = () => ({ org: props.org, repo: props.repo, epoch: props.epoch });
  const [envs] = createResource(key, ({ org, repo }) => listEnvironments(org, repo));

  return (
    <Show when={!envs.loading} fallback={<p class="empty">loading…</p>}>
      <Show when={!envs.error} fallback={<p class="error">Could not load environments.</p>}>
        <Show
          when={(envs()?.length ?? 0) > 0}
          fallback={
            <p class="empty">
              This project may not deploy into any environment yet. Environments are
              org-level: an org admin creates one and makes {props.repo} eligible for it
              under Settings › Environments.
            </p>
          }
        >
          <For each={envs()}>
            {(env) => (
              <EnvPanel
                org={props.org}
                repo={props.repo}
                env={env}
                onTighten={() => props.onTighten(env.name)}
                onChanged={props.onChanged}
              />
            )}
          </For>
        </Show>
      </Show>
    </Show>
  );
}

// The history here is APPROVALS — what governance admitted into the
// environment — not what went live.
function EnvPanel(props: {
  org: string;
  repo: string;
  env: ProjectEnvironment;
  onTighten: () => void;
  onChanged: () => void;
}) {
  const key = () => ({ org: props.org, repo: props.repo, name: props.env.name });
  const [history] = createResource(key, ({ org, repo, name }) =>
    listApprovalHistory(org, repo, name).catch(() => []),
  );
  const [error, setError] = createSignal<string | null>(null);
  const effective = () => props.env.effective;

  const removeOverlay = async () => {
    if (
      !window.confirm(
        `Remove ${props.repo}'s overlay on ${props.env.name}?\n\nIts deploys fall back to the org baseline.`,
      )
    )
      return;
    setError(null);
    try {
      await deleteEnvironmentOverlay(props.org, props.env.name, props.repo);
      props.onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not remove the overlay.");
    }
  };

  return (
    <div class="panel env-panel">
      <div class="panel-h">
        <span>{props.env.name}</span>
        <span class="env-h-meta">
          <Show when={!props.env.eligible}>
            <span class="tok-role" title="An org admin withdrew this project's eligibility">
              not eligible
            </span>
          </Show>
          <Show when={(history() ?? []).length > 0}>
            <span class="subtle mono">last approved {history()![0]!.git_ref}</span>
          </Show>
        </span>
      </div>
      <div class="env-body">
        <div class="env-rules mono" title="What admission enforces for this project">
          <Icon icon="shield-check" size={13} /> {effective().approvers.length} approver
          {effective().approvers.length === 1 ? "" : "s"}
          <Show when={effective().approvers.length > 0}>
            {" "}({effective().approvers.join(", ")})
          </Show>
          <span class="dotsep">·</span> {refLayersLabel(effective().allowed_ref_layers)}
          <Show when={effective().wait_timer_s > 0}>
            <span class="dotsep">·</span>{" "}
            <span title="Carried, not yet enforced">{effective().wait_timer_s}s wait</span>
          </Show>
          <Show when={effective().require_reason}>
            <span class="dotsep">·</span> reason required
          </Show>
        </div>
        <div class="env-layers">
          <div class="env-h-row mono">
            <span class="subtle">org baseline</span>{" "}
            <Show
              when={!isPermissive(props.env.baseline)}
              fallback={<span class="subtle">permissive — no org-wide rule yet</span>}
            >
              {rulesSummary(props.env.baseline)}
            </Show>
          </div>
          <div class="env-h-row mono">
            <span class="subtle">{props.repo} overlay</span>{" "}
            <Show when={props.env.overlay} fallback={<span class="subtle">none</span>}>
              {(o) => rulesSummary(o())}
            </Show>
            <span class="oenv-actions">
              <Show when={props.env.eligible || props.env.overlay}>
                <button class="btn btn-ghost btn-sm" onClick={props.onTighten}>
                  {props.env.overlay ? "edit" : "tighten"}
                </button>
              </Show>
              <Show when={props.env.overlay}>
                <button class="btn btn-ghost btn-sm" onClick={removeOverlay}>
                  remove
                </button>
              </Show>
            </span>
          </div>
          <Show when={props.env.privilege_grant}>
            {(g) => (
              <div class="env-h-row mono">
                <span class="subtle">privileges</span> {g().privileged_images.length} image
                {g().privileged_images.length === 1 ? "" : "s"}
                <Show when={g().permit_k8s_overlay}> · raw k8s_overlay allowed</Show>
              </div>
            )}
          </Show>
        </div>
        <Show when={error()}>
          <p class="error">{error()}</p>
        </Show>
        <div class="env-history">
          <div class="env-h-title mono">Approval history (legacy)</div>
          <For
            each={history() ?? []}
            fallback={<div class="env-h-row mono subtle">no approvals recorded</div>}
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

// ---- "Tighten for this project" ---------------------------------------------
// Writes this project's overlay (`PUT /v1/orgs/{org}/environments/{env}/
// projects/{repo}/overlay`, ADR-0071 §3). Offers only the Environments the
// project may deploy into (or already has an overlay in): an Environment is
// created, and made available, by an org admin. Every field can only make
// this project's deploys stricter than the org baseline.

function EnvDialog(props: {
  org: string;
  repo: string;
  /** Open on this Environment, its overlay prefilled. */
  initial?: string;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [envs] = createResource(
    () => ({ org: props.org, repo: props.repo }),
    ({ org, repo }) => listEnvironments(org, repo),
  );
  const choices = () => tightenable(envs() ?? []);
  const [picked, setPicked] = createSignal(props.initial ?? "");
  const blank = rulesToForm(null);
  const [approvers, setApprovers] = createSignal(blank.approvers);
  const [allowedRefs, setAllowedRefs] = createSignal(blank.allowedRefs);
  const [waitTimer, setWaitTimer] = createSignal(blank.waitTimerS);
  const [requireReason, setRequireReason] = createSignal(blank.requireReason);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  // Prefill from the picked Environment's overlay once the list is in (and
  // again on every pick): editing starts from what is stored, never blank.
  createEffect(() => {
    const env = (envs() ?? []).find((e) => e.name === picked());
    const form = rulesToForm(env?.overlay);
    setApprovers(form.approvers);
    setAllowedRefs(form.allowedRefs);
    setWaitTimer(form.waitTimerS);
    setRequireReason(form.requireReason);
  });

  const built = () =>
    toProtectionBody({
      approvers: approvers(),
      allowedRefs: allowedRefs(),
      waitTimerS: waitTimer(),
      requireReason: requireReason(),
    });
  const problem = () => (picked() ? (built().error ?? null) : "Pick an environment.");

  const save = async () => {
    const body = built().body;
    if (!body || problem() || saving()) return;
    setSaving(true);
    setError(null);
    try {
      await putEnvironmentOverlay(props.org, picked(), props.repo, body);
      props.onSaved();
      props.onClose();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not save the overlay.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <div class="modal-scrim" onClick={props.onClose}>
      <div class="modal" onClick={(e) => e.stopPropagation()}>
        <div class="panel-h">
          <span>Tighten for {props.repo}</span>
        </div>
        <div class="modal-body">
          <div class="form-r">
            <label>environment</label>
            <Show
              when={choices().length > 0}
              fallback={
                <p class="fieldnote">
                  {envs.loading
                    ? "loading…"
                    : `${props.repo} may not deploy into any environment yet — an org admin makes it eligible under Settings › Environments.`}
                </p>
              }
            >
              <select
                class="input mono"
                value={picked()}
                onChange={(e) => setPicked(e.currentTarget.value)}
              >
                <option value="">environment…</option>
                <For each={choices()}>
                  {(env) => (
                    <option value={env.name}>
                      {env.name}
                      {env.overlay ? " (has overlay)" : ""}
                    </option>
                  )}
                </For>
              </select>
            </Show>
          </div>
          <div class="form-r">
            <label>extra approvers</label>
            <input
              class="input"
              placeholder="comma-separated user ids (blank = none)"
              value={approvers()}
              onInput={(e) => setApprovers(e.currentTarget.value)}
            />
            {/* Approvers match a user id exactly and must name someone who
                exists (ADR-0069 §3), so offer the search rather than leave
                people to guess `github:alice` vs `alice`. */}
            <PrincipalSearch
              org={props.org}
              placeholder="add an approver — find a user"
              onPick={(id) => setApprovers(addToList(approvers(), id))}
            />
            <p class="fieldnote">Required in addition to the org baseline's approvers.</p>
          </div>
          <div class="form-r">
            <label>allowed refs</label>
            <input
              class="input"
              placeholder="comma-separated globs, e.g. refs/heads/main (blank = any)"
              value={allowedRefs()}
              onInput={(e) => setAllowedRefs(e.currentTarget.value)}
            />
            <p class="fieldnote">
              A ref must match these <b>and</b> the baseline's. Use full refs —{" "}
              <span class="mono">refs/heads/main</span>, not <span class="mono">main</span>.
            </p>
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
            <p class="fieldnote">Recorded, not yet enforced.</p>
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
          <Show when={problem() && picked()}>
            <p class="error">{problem()}</p>
          </Show>
          <Show when={error()}>
            <p class="error">{error()}</p>
          </Show>
          <div class="modal-actions">
            <button class="btn btn-primary" disabled={!!problem() || saving()} onClick={save}>
              <Icon icon="shield-check" size={14} /> {saving() ? "Saving…" : "Save overlay"}
            </button>
            <button class="btn btn-ghost" onClick={props.onClose}>Cancel</button>
          </div>
          <p class="subtle modal-note">
            An overlay can only make {props.repo}'s deploys stricter than the org baseline.
            Requires the Administer capability on this repo (ADR-0071).
          </p>
        </div>
      </div>
    </div>
  );
}
