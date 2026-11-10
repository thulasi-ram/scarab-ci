// Repo dashboard (tier 2) — tabs over one repository: Runs (default),
// Environments, Secrets, Settings. Each run row shows its origin as a grid of
// facts (author · ref · duration) under a header line (sha · status · trigger ·
// when), fed by the origin fields RunSummaryDto now carries. Duration ticks live
// for in-flight runs. The runs list filters by status, author, branch, PR,
// pipeline, environment and deploys — environment and deploys on the server
// (it is how a Project's older deploys are found), the rest over
// the loaded page. The header CTA is contextual to the active tab.
//
// The active tab and every filter live in the URL query (`?tab=secrets`,
// `?pipeline=ci&env=production`), not in component signals — so a refresh, a
// shared link and the back button all land where you were. The defaults (Runs
// tab, no filter) are the absent params, which keeps the bare repo URL clean.
import { createResource, createSignal, createEffect, onCleanup, For, Show } from "solid-js";
import { A, useParams, useNavigate, useSearchParams } from "@solidjs/router";
import { recordVisit } from "../visited";
import {
  listRepoRuns,
  repoForgeUrl,
  listEnvironments,
  putEnvironmentOverlay,
  deleteEnvironmentOverlay,
  type RunSummary,
  type ProjectEnvironment,
  listRefusedRuns,
  type RefusedRun,
  listSchedules,
  type Schedule,
} from "../api/client";
import {
  isPermissive,
  refLayersLabel,
  rulesSummary,
  rulesToForm,
  tightenable,
  toProtectionBody,
} from "../environment-overlays";
import { relTime, absTime, untilTime, duration } from "../fmt";
import { forgeCommitUrl, forgePrUrl } from "../forge";
import TriggerCell from "../components/TriggerCell";
import RunNumber from "../components/RunNumber";
import Icon from "../components/Icon";
import SearchSelect from "../components/SearchSelect";
import PrincipalSearch from "../components/PrincipalSearch";
import Modal, { ModalMessage, ModalSection } from "../components/Modal";
import ConfirmDestructive from "../components/ConfirmDestructive";
import { Checkbox, Field, Input, Select } from "../components/Field";
import { addToList } from "../identities";
import SecretKeyList from "../components/SecretKeyList";
import RepoSettings from "../components/RepoSettings";
import Doodle from "../components/Doodle";
import AsciiScene from "../components/AsciiScene";
import { mergeRefused, runEnvironmentOptions, runListFilter } from "../runs-filter";
import { environmentLabel, nameBeside, shownName } from "../org-environments";
import { refusalSummary } from "../compile-errors";
import { fireOutcome, pipelineShortName } from "../schedules";
import ponderIdle from "../../../brand/ascii/generated/ponder-ponder.json";

type Tab = "runs" | "environments" | "secrets" | "settings";
type StatusFilter = "all" | "running" | "failed" | "refused";

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
    return v === "running" || v === "failed" || v === "refused" ? v : "all";
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
  // `deploy: true`. Server-side, like the Environment.
  const deploysOnly = () => one(search.deploy) === "1";
  // The Secrets tab's Environment (`?secret_env=`); its own key, so the Runs
  // tab's Environment filter does not carry over.
  const secretEnv = () => one(search.secret_env);
  const setSecretEnv = filterSetter("secret_env");
  const setDeploysOnly = (on: boolean) => filterSetter("deploy")(on ? "1" : "");

  // Only this repo's tenanted runs, straight from the per-repo endpoint.
  // The Environment and Deploys filters are part of the request,
  // so they fetch the newest matching runs rather than filtering the newest
  // page.
  const [rows, { refetch }] = createResource(
    () => ({ org: org(), repo: repo(), env: envFilter(), deploys: deploysOnly() }),
    (k): Promise<RunSummary[]> =>
      listRepoRuns(k.org, k.repo, 50, runListFilter(k.env, k.deploys)),
  );
  // Triggers that did not become a Run: a compile error, a yanked
  // library, a Policy. Shown between the Runs; an older server has none.
  const [refused, { refetch: refetchRefused }] = createResource(
    () => ({ org: org(), repo: repo() }),
    (k) => listRefusedRuns(k.org, k.repo).catch(() => [] as RefusedRun[]),
  );
  // The Project's `on: cron:` schedules; an older server has none, and a repo
  // without one shows no Schedules panel.
  const [schedules, { refetch: refetchSchedules }] = createResource(
    () => ({ org: org(), repo: repo() }),
    (k) => listSchedules(k.org, k.repo).catch(() => [] as Schedule[]),
  );
  // The Project's Environments, for the Environment picker's options — the
  // loaded rows are already one Environment's once the filter is on. A caller
  // who can't read them still gets the Environments seen on runs.
  const [projectEnvRecords] = createResource(
    () => ({ org: org(), repo: repo() }),
    (k) => listEnvironments(k.org, k.repo).catch(() => [] as ProjectEnvironment[]),
  );
  const projectEnvs = () => (projectEnvRecords() ?? []).map((e) => e.name);
  /** The picker's label for an Environment name: its display name with the
   *  name beside it, or the name alone (an Environment seen only on runs). */
  const envLabel = (name: string) => {
    const env = (projectEnvRecords() ?? []).find((e) => e.name === name);
    return env ? environmentLabel(env) : name;
  };

  /** `undefined` = closed; a name = open on that Environment (its overlay
   *  prefilled). */
  const [envDialog, setEnvDialog] = createSignal<string | undefined>(undefined);
  const [envEpoch, setEnvEpoch] = createSignal(0); // bump to refetch the env list after a create

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

  // The rows on screen: the filtered Runs with refused triggers between them.
  const listRows = () =>
    mergeRefused(filtered(), refused() ?? [], {
      status: statusFilter(),
      author: authorFilter(),
      pipeline: pipelineFilter(),
      runOnly: !!branchFilter() || !!prFilter() || !!envFilter() || deploysOnly(),
    });

  // The header CTA follows the tab: what you'd create in this context.
  const cta = () => {
    switch (tab()) {
      case "runs":
        return {
          label: "Run pipeline",
          icon: "play",
          onClick: () => nav(`/${org()}/${repo()}/run`),
        };
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
        <Show when={(schedules() ?? []).length > 0}>
          <div class="panel">
            <div class="panel-h">
              <span>Schedules</span>
              <span class="tok-count">{schedules()!.length}</span>
            </div>
            <div class="sched-body">
              <For each={schedules()}>
                {(s) => <ScheduleRow org={org()} repo={repo()} s={s} />}
              </For>
            </div>
          </div>
        </Show>

        <div class="filters">
          <For each={["all", "running", "failed", "refused"] as const}>
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
              options={environments().map((e) => ({ value: e, label: envLabel(e) }))}
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

          <button
            class="btn btn-ghost btn-sm filters-refresh"
            onClick={() => {
              refetch();
              refetchRefused();
              refetchSchedules();
            }}
          >
            <Icon icon="rotate-cw" size={13} /> Refresh
          </button>
        </div>

        <Show when={!rows.loading} fallback={<p class="empty">loading…</p>}>
          <Show when={!rows.error} fallback={<p class="error">Could not load runs.</p>}>
            <Show
              when={listRows().length > 0}
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
                <For each={listRows()}>
                  {(row) => {
                    if (row.kind === "refused") return <RefusedRow r={row.refused} />;
                    const r = row.run;
                    const commitUrl = () => forgeCommitUrl(repoUrl(), r.sha);
                    const prUrl = () => forgePrUrl(repoUrl(), r.pr_number);
                    const stop = (e: MouseEvent) => e.stopPropagation();
                    return (
                      <div class="runrow" onClick={() => nav(`/${org()}/${repo()}/runs/${r.id}`)}>
                        {/* Run-number gutter: the per-repo `#N`
                            handle, sequential so it scans down the list. */}
                        <div class="rr-gutter">
                          <RunNumber n={r.run_number} id={r.id} />
                        </div>
                        <div class="rr-body">
                          {/* Primary line: the run Headline (trigger kind + title),
                              the SAME shared cell the run-detail bar uses. */}
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
                            {/* base ← head on PR runs; else the branch/ref. */}
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
        <SecretKeyList
          org={org()}
          repo={repo()}
          environment={secretEnv()}
          onEnvironment={setSecretEnv}
        />
      </Show>

      {/* ---- Settings ---- */}
      <Show when={tab() === "settings"}>
        <RepoSettings org={org()} repo={repo()} repoUrl={repoUrl() ?? null} />
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


// ---- environments (org Environments as this project sees them) ----------
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
              This project may not use any environment yet. Environments are
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

// What ran in the environment lives on the Runs tab (filtered by it), so the
// panel shows the rules only and links there.
function EnvPanel(props: {
  org: string;
  repo: string;
  env: ProjectEnvironment;
  onTighten: () => void;
  onChanged: () => void;
}) {
  const [error, setError] = createSignal<string | null>(null);
  const [removing, setRemoving] = createSignal(false);
  const effective = () => props.env.effective;

  return (
    <div class="panel env-panel">
      <div class="panel-h">
        <span>
          {shownName(props.env)}
          <Show when={nameBeside(props.env)}>
            {(name) => (
              <span class="oenv-name mono" title="The name pipelines use">
                {" "}
                {name()}
              </span>
            )}
          </Show>
        </span>
        <span class="env-h-meta">
          <Show when={!props.env.eligible}>
            <span class="tok-role" title="An org admin withdrew this project's eligibility">
              not eligible
            </span>
          </Show>
          <A
            class="subtle mono"
            href={`/${props.org}/${props.repo}?env=${encodeURIComponent(props.env.name)}`}
          >
            runs in {props.env.name} →
          </A>
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
            <span
              title={
                effective().approvers.length > 0
                  ? "A minimum wait on the approval gate, after the approvals are in"
                  : "Does nothing: the timer needs approvers"
              }
            >
              {effective().wait_timer_s}s wait
              {effective().approvers.length > 0 ? "" : " (no approvers: does nothing)"}
            </span>
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
                <button class="btn btn-ghost btn-sm" onClick={() => setRemoving(true)}>
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
        <Show when={removing()}>
          <ConfirmDestructive
            eyebrow={`${props.org}/${props.repo} · environments`}
            title={`Remove ${props.repo}'s overlay on ${props.env.name}?`}
            purpose="Its deploys there fall back to the org baseline."
            name={props.env.name}
            confirmLabel="Remove overlay"
            onConfirm={async () => {
              setError(null);
              await deleteEnvironmentOverlay(props.org, props.env.name, props.repo);
              props.onChanged();
            }}
            onClose={() => setRemoving(false)}
          >
            <Show when={props.env.overlay}>
              {(o) => (
                <p class="dlg-ctx">
                  <span class="k">overlay now</span> {rulesSummary(o())}
                </p>
              )}
            </Show>
          </ConfirmDestructive>
        </Show>
      </div>
    </div>
  );
}

// ---- "Tighten for this project" ---------------------------------------------
// Writes this project's overlay (`PUT /v1/orgs/{org}/environments/{env}/
// projects/{repo}/overlay`). Offers only the Environments the
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

  const pickedEnv = () => (envs() ?? []).find((e) => e.name === picked());
  const waitError = () => (picked() ? (built().error ?? null) : null);
  // The footer's "after saving" line: the two layers a deploy must pass, as
  // stored. The effective rules are the server's to compute, not merged here.
  const afterSaving = () => {
    const body = built().body;
    if (!picked() || !body) return undefined;
    return {
      value: isPermissive(body)
        ? `org baseline only: this overlay adds no rule`
        : `org baseline, plus ${rulesSummary(body)}`,
    };
  };

  return (
    <Modal
      eyebrow={`${props.org}/${props.repo} · environments`}
      title={picked() ? `Tighten ${picked()} for ${props.repo}` : `Tighten for ${props.repo}`}
      purpose={`Rules set here apply only to ${props.repo}, and can only make its deploys stricter than the org baseline.`}
      busy={saving()}
      onClose={props.onClose}
      after={afterSaving()}
      footer={
        <>
          <Show when={error()}>
            <ModalMessage>{error()}</ModalMessage>
          </Show>
          <button class="btn btn-ghost" type="button" onClick={props.onClose} disabled={saving()}>
            Cancel
          </button>
          <button class="btn btn-primary" type="button" disabled={!!problem() || saving()} onClick={save}>
            <Icon icon="shield-check" size={14} /> {saving() ? "Saving…" : "Save overlay"}
          </button>
        </>
      }
    >
      <ModalSection n={1} title="Target">
        <Show
          when={choices().length > 0}
          fallback={
            <p class="dlg-text">
              {envs.loading
                ? "loading…"
                : `${props.repo} may not use any environment yet. An org admin makes it eligible under Settings › Environments.`}
            </p>
          }
        >
          <Field label="Environment" required help={`Environments ${props.repo} may use.`}>
            <Select mono value={picked()} onValue={setPicked}>
              <option value="">environment…</option>
              <For each={choices()}>
                {(env) => (
                  <option value={env.name}>
                    {environmentLabel(env)}
                    {env.overlay ? " (has overlay)" : ""}
                  </option>
                )}
              </For>
            </Select>
          </Field>
          <Show when={pickedEnv()}>
            {(env) => (
              <p class="dlg-ctx">
                <span class="k">org baseline</span>{" "}
                {isPermissive(env().baseline) ? "no rule yet" : rulesSummary(env().baseline)}
              </p>
            )}
          </Show>
        </Show>
      </ModalSection>
      <ModalSection n={2} title="Approvers">
        <Field label="Extra approvers" help="Required in addition to the baseline's approvers. User ids, comma-separated.">
          <Input mono value={approvers()} onValue={setApprovers} placeholder="none" />
        </Field>
        {/* Approvers match a user id exactly and must name someone who
            exists, so offer the search rather than leave people to guess
            `github:alice` vs `alice`. */}
        <PrincipalSearch
          org={props.org}
          placeholder="Add an approver: find a user"
          onPick={(id) => setApprovers(addToList(approvers(), id))}
        />
      </ModalSection>
      <ModalSection n={3} title="Refs and timing">
        <Field
          label="Allowed refs"
          help={
            <>
              A ref must match these and the baseline's. Use full refs:{" "}
              <span class="mono">refs/heads/main</span>, not <span class="mono">main</span>.
            </>
          }
        >
          <Input mono value={allowedRefs()} onValue={setAllowedRefs} placeholder="blank allows any ref" />
        </Field>
        <Field label="Wait timer" help="A minimum wait on the approval gate. The longer of this and the baseline's applies, and only when there are approvers." error={waitError()}>
          <Input mono inputmode="numeric" suffix="seconds" value={waitTimer()} onValue={setWaitTimer} />
        </Field>
        <Checkbox
          checked={requireReason()}
          onChecked={setRequireReason}
          label="Require a reason"
          help="Manual and API dispatches must give one."
        />
      </ModalSection>
    </Modal>
  );
}

/** One `on: cron:` schedule: its pipeline, the expression (UTC), the next
 *  slot, and how the Pipeline's last slot came out, linked to its Run. */
function ScheduleRow(props: { org: string; repo: string; s: Schedule }) {
  const s = () => props.s;
  return (
    <div class="rr-facts">
      <span class="pfact strong" title={s().pipeline}>
        <Icon icon="workflow" size={12} />
        <span class="mono">{pipelineShortName(s().pipeline)}</span>
      </span>
      <span class="pfact" title={`read from ${s().source_sha.slice(0, 7)} on the default branch`}>
        <Icon icon="clock" size={12} />
        <span class="mono">{s().schedule}</span>
        <span>UTC</span>
      </span>
      <span class="pfact" title={absTime(s().next_fire_at)}>
        <Icon icon="timer" size={12} />
        <span>next {untilTime(s().next_fire_at)}</span>
      </span>
      <Show
        when={s().last_fire}
        fallback={<span class="pfact subtle">not fired yet</span>}
      >
        {(f) => {
          const o = () => fireOutcome(f().outcome);
          return (
            <>
              <span class="pfact" title={absTime(f().slot_at)}>
                <Icon icon="history" size={12} />
                <span>last {relTime(f().slot_at)}</span>
                <Show when={f().schedule !== s().schedule}>
                  <span class="mono" title="the schedule that fired">({f().schedule})</span>
                </Show>
              </span>
              <span class={`rr-status ${o().tone}`}>{o().label}</span>
              <Show when={f().run_id}>
                {(id) => (
                  <A class="pfact link" href={`/${props.org}/${props.repo}/runs/${id()}`}>
                    <Icon icon="play" size={12} />
                    <span>open the run</span>
                  </A>
                )}
              </Show>
            </>
          );
        }}
      </Show>
    </div>
  );
}

/** A trigger that did not become a Run: where a Run would be,
 *  with the compact reason and its fix. */
function RefusedRow(props: { r: RefusedRun }) {
  const r = () => props.r;
  const sum = () => refusalSummary(r().errors, r().message);
  const [all, setAll] = createSignal(false);
  const branch = () => r().git_ref?.replace(/^refs\/(heads|tags)\//, "");
  return (
    <div class="runrow refused">
      <div class="rr-gutter">
        <span class="runnum none">
          <span class="n">no run</span>
        </span>
      </div>
      <div class="rr-body">
        <div class="rr-head">
          <TriggerCell kind={r().trigger_kind} title={r().title} />
        </div>
        <div class="rr-facts">
          <span class="pfact strong" title="pipeline">
            <Icon icon="workflow" size={12} />
            <span class="mono">{pipelineShortName(r().pipeline)}</span>
          </span>
          <Show when={r().sha}>
            {(sha) => (
              <span class="pfact" title="commit">
                <Icon icon="git-commit-horizontal" size={12} />
                <span class="mono sha">{sha().slice(0, 7)}</span>
              </span>
            )}
          </Show>
          <Show when={branch()}>
            {(b) => (
              <span class="pfact" title="branch / ref">
                <Icon icon="git-branch" size={12} />
                <span class="mono">{b()}</span>
              </span>
            )}
          </Show>
          <Show when={r().actor}>
            {(a) => (
              <span class="pfact" title="triggered by">
                <Icon icon="user" size={12} />
                <span>{a()}</span>
              </span>
            )}
          </Show>
          <span class="pfact subtle" title={absTime(r().created_at)}>
            {relTime(r().created_at)}
          </span>
        </div>
        <div class="rf">
          <div class="rf-what">
            <Icon icon={r().reason_kind === "compile" ? "circle-x" : "ban"} size={13} />
            <span>{sum().what}</span>
            <Show when={sum().more > 0 && !all()}>
              <span class="why">
                · {sum().more} more {sum().more === 1 ? "error" : "errors"}
              </span>
            </Show>
          </div>
          <Show when={sum().fix}>
            {(fix) => (
              <div class="rf-fix">
                <span class="lbl">fix</span>
                <span>{fix()}</span>
              </div>
            )}
          </Show>
          <Show when={r().reason_kind === "yanked"}>
            <div class="rf-fix">
              <span class="lbl">fix</span>
              <span>an org admin upgrades the installation, then push again</span>
            </div>
          </Show>
          <Show when={sum().more > 0}>
            <div class="rf-fix">
              <button class="xs-link" onClick={() => setAll((v) => !v)}>
                {all() ? "Hide the other errors" : `Show all ${r().errors.length} errors`}
              </button>
            </div>
            <Show when={all()}>
              <For each={r().errors.slice(1)}>
                {(e) => (
                  <div class="rf-what">
                    <Icon icon="circle-x" size={13} />
                    <span>{refusalSummary([e], e.message).what}</span>
                  </div>
                )}
              </For>
            </Show>
          </Show>
        </div>
      </div>
      <div class="rr-right">
        <span class="rr-status refused">refused</span>
      </div>
    </div>
  );
}
