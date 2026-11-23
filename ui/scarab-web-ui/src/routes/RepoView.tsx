// Repo dashboard (tier 2) — tabs over one repository: Runs (default),
// Schedules, Environments, Secrets, Settings. The header's button is the
// active tab's action (Run pipeline on Runs).
//
// The runs list is grouped by day; each row reads `pipeline / headline` over
// line 2 (the trigger, where the Run came from, its commit and its author),
// with its status and clock time on the right (see components/RunRow). A Run
// a newer one replaced sits under its replacer. Over the list: a status
// control with counts (All, Running, Waiting, Failed, Refused) and one Filter
// button whose panel holds pipeline, environment, author, branch, PR, deploys
// and scheduled; the filters that are on show as removable chips. Environment
// and deploys filter on the server (it is how a Project's older deploys are
// found), the rest over the loaded page. The list polls every 10 seconds
// while the page is visible; rows on screen update in place, and Runs that
// arrive wait behind a `N new Runs` pill so the list does not move.
//
// The active tab and every filter live in the URL query (`?tab=secrets`,
// `?pipeline=ci&env=production`), not in component signals — so a refresh, a
// shared link and the back button all land where you were. The defaults (Runs
// tab, no filter) are the absent params, which keeps the bare repo URL clean.
import {
  createEffect,
  createMemo,
  createResource,
  createSignal,
  createUniqueId,
  For,
  Match,
  onCleanup,
  Show,
  Switch,
  untrack,
} from "solid-js";
import { A, useParams, useNavigate, useSearchParams } from "@solidjs/router";
import { orgPageHref, UNREACHABLE_NOTE } from "../org-page";
import { recordVisit } from "../visited";
import {
  listRepoRuns,
  listProjects,
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
  rulesSummary,
  rulesToForm,
  tightenable,
  toProtectionBody,
} from "../environment-overlays";
import {
  approversCell,
  deploySentence,
  ruleDiff,
  refsCell,
  repoChip,
  waitCell,
} from "../repo-environments";
import { relTime, absTime, untilTime, duration } from "../fmt";
import { forgeCommitUrl, forgePrUrl } from "../forge";
import {
  arrivedLabel,
  authorSlot,
  clockTime,
  dayGroups,
  daySummary,
  deployTag,
  kindFirst,
  placeReplaced,
  pullNumber,
  rowStatus,
  splitArrived,
  type DayGroup,
  type Placed,
  type Replaced,
} from "../run-row";
import { statusWord } from "../run-page";
import { OriginLine, RowStatus, RowTitle, SkeletonRow, StatusMark } from "../components/RunRow";
import RunNumber from "../components/RunNumber";
import Icon from "../components/Icon";
import { HomeCrumb, RepoCrumb } from "../components/PageCrumbs";
import SearchSelect, { type SelectOption } from "../components/SearchSelect";
import PrincipalSearch from "../components/PrincipalSearch";
import Modal, { ModalMessage, ModalSection } from "../components/Modal";
import ConfirmDestructive from "../components/ConfirmDestructive";
import { Checkbox, Field, Input, Select } from "../components/Field";
import { addToList } from "../identities";
import SecretKeyList from "../components/SecretKeyList";
import RepoSettings from "../components/RepoSettings";
import Doodle from "../components/Doodle";
import AsciiScene from "../components/AsciiScene";
import {
  filterChips,
  noMatchSentence,
  parseSegment,
  runEnvironmentOptions,
  runListFilter,
  segmentCounts,
  segmentRows,
  SEGMENTS,
  type PanelFilters,
  type RowFilter,
  type RunListRow,
  type StatusSegment,
} from "../runs-filter";
import { environmentLabel, nameBeside, shownName } from "../org-environments";
import { refusalSummary } from "../compile-errors";
import { fireOutcome, isScheduled, pipelineShortName, scheduledRunsHref } from "../schedules";
import ponderIdle from "../../../brand/ascii/generated/ponder-ponder.json";
import Pill from "../components/Pill";

/** One loaded list and when: `items` is null until a load succeeds, and
 *  a failed poll keeps the last items of the same request (`key`). */
type Feed<T> = { key: string; items: T[] | null; at: number | null; failedAt: number | null };
/** A row of the list: a Run, or a trigger that was refused. */
type Row = RunListRow<RunSummary, RefusedRun>;
/** One entry of the flattened list: a day header or a row. */
type Entry = { t: "day"; g: DayGroup<Row> } | { t: "row"; item: Placed<Row> };
/** A row's key, stable across polls. */
const listKey = (row: Row): string => (row.kind === "run" ? `r:${row.run.id}` : `x:${row.refused.id}`);

/** How many Runs the list loads (the newest), and how often it polls. */
const RUNS_LIMIT = 50;
const POLL_MS = 10_000;

/** The pipeline guide, and the smallest pipeline file, for a repo that has
 *  not run yet. */
const PIPELINE_GUIDE = "https://thulasi-ram.github.io/scarab-ci/usage/pipeline-syntax/";
const FIRST_PIPELINE = `.scarab/ci.yaml

on:
  push: {}
steps:
  - id: test
    image: rust:1-bookworm
    command: [cargo, test]`;

type Tab = "runs" | "schedules" | "environments" | "secrets" | "settings";

const TABS: [Tab, string][] = [
  ["runs", "Runs"],
  ["schedules", "Schedules"],
  ["environments", "Environments"],
  ["secrets", "Secrets"],
  ["settings", "Settings"],
];

// A run is "live" (still accruing wall time) until it reaches a terminal status.
const TERMINAL = new Set(["succeeded", "failed", "cancelled", "dead_lettered"]);
const isLive = (status: string) => !TERMINAL.has(status);

// The branch/tag a run ran on: `git_ref` with its `refs/{heads,tags}/` prefix
// stripped, for the branch filter. `null` when refless.
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

  // This repo's Project, for whether its forge connection still reaches it.
  const [project] = createResource(
    () => ({ org: org(), repo: repo() }),
    (k) =>
      listProjects()
        .then((ps) => ps.find((p) => p.org === k.org && p.project === k.repo) ?? null)
        .catch(() => null),
  );
  const unreachable = () => project()?.unreachable_since != null;

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
  const segment = (): StatusSegment => parseSegment(one(search.status));
  const setSegment = (f: StatusSegment) => filterSetter("status")(f === "all" ? "" : f);
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
  // Scheduled toggle (`?scheduled=1`): only Runs a schedule started (trigger
  // kind `cron`). Client-side, like the pipeline; a schedule on the Schedules
  // tab links here with its pipeline.
  const scheduledOnly = () => one(search.scheduled) === "1";
  const setScheduledOnly = (on: boolean) => filterSetter("scheduled")(on ? "1" : "");

  // Only this repo's tenanted runs, straight from the per-repo endpoint.
  // The Environment and Deploys filters are part of the request,
  // so they fetch the newest matching runs rather than filtering the newest
  // page. The fetch never fails the resource: a failed poll keeps the last
  // list that loaded (for the same request) and says when it loaded.
  const feedKey = (k: { org: string; repo: string; env: string; deploys: boolean }) =>
    `${k.org}/${k.repo}?env=${k.env}&deploy=${k.deploys}`;
  const runsRequest = () => ({ org: org(), repo: repo(), env: envFilter(), deploys: deploysOnly() });
  const [feed, { refetch }] = createResource(
    runsRequest,
    async (k, info): Promise<Feed<RunSummary>> => {
      const key = feedKey(k);
      try {
        const items = await listRepoRuns(k.org, k.repo, RUNS_LIMIT, runListFilter(k.env, k.deploys));
        return { key, items, at: Date.now(), failedAt: null };
      } catch {
        const prev = info.value?.key === key ? info.value : null;
        return { key, items: prev?.items ?? null, at: prev?.at ?? null, failedAt: Date.now() };
      }
    },
  );
  // Triggers that did not become a Run: a compile error, a yanked
  // library, a Policy. Shown between the Runs; an older server has none.
  // A failed poll keeps the last ones that loaded.
  const [refusedFeed, { refetch: refetchRefused }] = createResource(
    () => ({ org: org(), repo: repo() }),
    async (k, info): Promise<Feed<RefusedRun>> => {
      const key = `${k.org}/${k.repo}`;
      try {
        return { key, items: await listRefusedRuns(k.org, k.repo), at: Date.now(), failedAt: null };
      } catch {
        const prev = info.value?.key === key ? info.value : null;
        return { key, items: prev?.items ?? [], at: prev?.at ?? null, failedAt: Date.now() };
      }
    },
  );
  // `.latest`, never a read that suspends: a poll must not blank the list.
  // A list loaded for another request (the Environment just changed) is not
  // shown while the new one loads.
  const current = () => {
    const f = feed.latest;
    return f && f.key === feedKey(runsRequest()) ? f : null;
  };
  const rows = (): RunSummary[] | null => current()?.items ?? null;
  const refused = (): RefusedRun[] => {
    const f = refusedFeed.latest;
    return f && f.key === `${org()}/${repo()}` ? (f.items ?? []) : [];
  };

  // Runs and refused triggers that arrive while the list is read wait
  // behind the `N new Runs` pill. `seen` holds the keys on screen, per
  // request: a new request (another repo, Environment or Deploys) shows all
  // it loads.
  const [seenRuns, setSeenRuns] = createSignal<{ key: string; ids: Set<string> } | null>(null);
  const [seenRefused, setSeenRefused] = createSignal<{ key: string; ids: Set<string> } | null>(null);
  createEffect(() => {
    const f = feed.latest;
    if (!f?.items) return;
    if (untrack(seenRuns)?.key !== f.key) setSeenRuns({ key: f.key, ids: new Set(f.items.map((r) => r.id)) });
  });
  createEffect(() => {
    const f = refusedFeed.latest;
    if (!f) return;
    if (untrack(seenRefused)?.key !== f.key) setSeenRefused({ key: f.key, ids: new Set(f.items?.map((r) => String(r.id))) });
  });
  // Until `seen` catches up with a new request, every row is on screen.
  const shownRuns = () => {
    const s = seenRuns();
    return s && s.key === current()?.key ? splitArrived(rows() ?? [], (r) => r.id, s.ids).shown : (rows() ?? []);
  };
  const shownRefused = () => {
    const s = seenRefused();
    return s && s.key === `${org()}/${repo()}` ? splitArrived(refused(), (r) => String(r.id), s.ids).shown : refused();
  };
  /** Shows the rows that arrived: every loaded key is on screen. */
  const showArrived = () => {
    const f = feed.latest;
    if (f?.items) setSeenRuns({ key: f.key, ids: new Set(f.items.map((r) => r.id)) });
    const g = refusedFeed.latest;
    if (g) setSeenRefused({ key: g.key, ids: new Set(g.items?.map((r) => String(r.id))) });
  };

  // Live: poll both lists every 10s while the page is visible and on the
  // Runs tab, and once when it becomes visible again.
  const poll = () => {
    if (tab() !== "runs" || document.visibilityState !== "visible") return;
    if (!feed.loading) refetch();
    if (!refusedFeed.loading) refetchRefused();
  };
  const pollTimer = setInterval(poll, POLL_MS);
  const onVisible = () => {
    if (document.visibilityState === "visible") poll();
  };
  document.addEventListener("visibilitychange", onVisible);
  onCleanup(() => {
    clearInterval(pollTimer);
    document.removeEventListener("visibilitychange", onVisible);
  });
  const retry = () => {
    refetch();
    refetchRefused();
  };

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

  // A 1s heartbeat so in-flight runs show a live, ticking elapsed time, and
  // `updated Ns ago` counts. Terminal runs use their frozen `duration_ms`.
  const [now, setNow] = createSignal(Date.now());
  const tick = setInterval(() => setNow(Date.now()), 1000);
  onCleanup(() => clearInterval(tick));

  // Option sets for the Filter panel's pickers, derived from whatever the
  // fetched rows carry — so you only ever pick a value that exists. A picker
  // with no values is left out unless it is set. Environments also take the
  // Project's own list, since the rows are filtered on the server.
  const authors = () => distinct(rows() ?? [], (r) => r.actor).sort();
  const branches = () => distinct(rows() ?? [], branchLabel).sort();
  const prNumbers = () => distinct(rows() ?? [], (r) => r.pr_number).sort((a, b) => b - a);
  const pipelines = () => distinct(rows() ?? [], (r) => r.pipeline).sort();
  const environments = () =>
    runEnvironmentOptions(projectEnvs() ?? [], rows() ?? [], envFilter());

  const panelFilters = (): PanelFilters => ({
    pipeline: pipelineFilter(),
    env: envFilter(),
    author: authorFilter(),
    branch: branchFilter(),
    pr: prFilter(),
    deploy: deploysOnly(),
    scheduled: scheduledOnly(),
  });
  const chips = () => filterChips(panelFilters());

  /** Clears the panel's filters; `status` too for the no-match state. */
  const clearFilters = (status = false) =>
    setSearch(
      {
        ...(status ? { status: "" } : {}),
        author: "", branch: "", pr: "", pipeline: "", env: "", deploy: "", scheduled: "",
      },
      { replace: true },
    );

  // Every filter but the status, ANDed. Each origin field is independently
  // nullable, so an engaged filter excludes any row that can't match it
  // (older runs predating origin-stamping fall out here), but a disengaged
  // one (`""`) never hides a row for lacking that field.
  const passes = (r: RunSummary) => {
    const author = authorFilter();
    const branch = branchFilter();
    const pr = prFilter();
    const pipeline = pipelineFilter();
    const env = envFilter();
    if (author && r.actor !== author) return false;
    if (branch && branchLabel(r) !== branch) return false;
    if (pr && String(r.pr_number ?? "") !== pr) return false;
    if (pipeline && r.pipeline !== pipeline) return false;
    // Already applied by the server; kept so a page still in flight from
    // the previous filter never shows under the new one.
    if (env && r.environment !== env) return false;
    if (deploysOnly() && !r.deploy) return false;
    if (scheduledOnly() && !isScheduled(r.trigger_kind)) return false;
    return true;
  };
  const rowFilter = (): RowFilter => ({
    author: authorFilter(),
    pipeline: pipelineFilter(),
    scheduled: scheduledOnly(),
    runOnly: !!branchFilter() || !!prFilter() || !!envFilter() || deploysOnly(),
  });

  // The rows on screen: the filtered Runs of the segment with refused
  // triggers between them. Counts are over the same rows.
  const listRows = createMemo(() => segmentRows(shownRuns().filter(passes), shownRefused(), segment(), rowFilter()));
  const counts = createMemo(() => segmentCounts(shownRuns().filter(passes), shownRefused(), rowFilter()));
  // How many rows of this view wait behind the pill.
  const arrived = createMemo(() => {
    const all = segmentRows((rows() ?? []).filter(passes), refused(), segment(), rowFilter());
    const on = new Set(listRows().map(listKey));
    return all.filter((r) => !on.has(listKey(r))).length;
  });

  // The rows in day groups, each replaced Run under the Run that replaced
  // it, flattened to keys: a row keeps its DOM across polls and updates in
  // place (a `<For>` over strings keys by value).
  const view = createMemo(() => {
    const groups = dayGroups(
      placeReplaced(listRows(), (row) => (row.kind === "run" ? row.run : null)),
      (row) => (row.kind === "run" ? row.run.status : "refused"),
      now(),
    );
    const order: string[] = [];
    const byKey = new Map<string, Entry>();
    for (const g of groups) {
      let dk = `d:${g.key}`;
      while (byKey.has(dk)) dk += "+";
      order.push(dk);
      byKey.set(dk, { t: "day", g });
      for (const item of g.items) {
        const k = listKey(item.row);
        order.push(k);
        byKey.set(k, { t: "row", item });
      }
    }
    return { order, byKey };
  });

  // The list's state: loading (nothing loaded yet), failed with nothing to
  // show, never ran, no match, or the list.
  const failedAt = () => current()?.failedAt ?? null;
  const loadedAt = () => current()?.at ?? null;
  const loading = () => rows() === null && !failedAt();
  const neverRan = () =>
    (rows() ?? []).length === 0 && refused().length === 0 && !envFilter() && !deploysOnly();

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

      {/* The heading row keeps the button's height on every tab, so the
          tabs never shift between tabs with and without it. */}
      <div class="page-head repo-head">
        <h1 class="crumb-head">
          <HomeCrumb size={20} sepClass="crumb-head-sep" />
          <RepoCrumb org={org()} repo={repo()} />
          <Show when={unreachable()}>
            <Pill tone="warn" look="dashed" class="repo-unreachable-tag" title={UNREACHABLE_NOTE}>
              unreachable
            </Pill>
          </Show>
        </h1>
        <Show when={cta()}>
          {(c) => (
            <button class="btn btn-primary repo-cta" aria-label={c().label} title={c().label} onClick={() => c().onClick()}>
              <Icon icon={c().icon} size={14} /> <span class="repo-cta-label">{c().label}</span>
            </button>
          )}
        </Show>
      </div>
      <Show when={unreachable()}>
        <p class="repo-unreachable-note">
          <Icon icon="alert-triangle" size={13} /> {UNREACHABLE_NOTE}{" "}
          <A href={orgPageHref(org())}>Organization page</A>
        </p>
      </Show>

      <div class="tabs repo-tabs">
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
        <div class="rl-bar">
          <div class="rl-segs" role="group" aria-label="Status">
            <For each={SEGMENTS}>
              {([key, label]) => (
                <button
                  type="button"
                  class="rl-seg"
                  classList={{ on: segment() === key }}
                  aria-pressed={segment() === key}
                  onClick={() => setSegment(key)}
                >
                  {label}
                  {/* No counts until the Runs load: the refused triggers
                      alone would count. */}
                  <Show when={rows() !== null}>
                    <span
                      class="rl-count"
                      classList={{ bad: (key === "failed" || key === "refused") && counts()[key] > 0 }}
                    >
                      {counts()[key]}
                    </span>
                  </Show>
                </button>
              )}
            </For>
          </div>
          <FilterPanel
            count={chips().length}
            pipeline={{ value: pipelineFilter(), set: setPipelineFilter, options: pipelines().map((p) => ({ value: p, label: p })) }}
            environment={{ value: envFilter(), set: setEnvFilter, options: environments().map((e) => ({ value: e, label: envLabel(e) })) }}
            author={{ value: authorFilter(), set: setAuthorFilter, options: authors().map((a) => ({ value: a, label: a })) }}
            branch={{ value: branchFilter(), set: setBranchFilter, options: branches().map((b) => ({ value: b, label: b })) }}
            pr={{ value: prFilter(), set: setPrFilter, options: prNumbers().map((p) => ({ value: String(p), label: `pr #${p}` })) }}
            deploys={deploysOnly()}
            onDeploys={setDeploysOnly}
            scheduled={scheduledOnly()}
            onScheduled={setScheduledOnly}
          />
          <span class="rl-spacer" />
          <Show when={loadedAt()}>
            {(at) => (
              <span
                class="rl-live"
                role="status"
                title="The list checks for new Runs every 10 seconds while this page is open"
              >
                <span class="rl-pulse" aria-hidden="true" />
                <span class="rl-live-text">Live · updated {(now(), relTime(at()))}</span>
              </span>
            )}
          </Show>
        </div>
        <Show when={chips().length > 0}>
          <div class="rl-chips">
            <For each={chips()}>
              {(c) => (
                <span class="rl-chip" title={c.full ?? undefined}>
                  <span class="rl-chip-k">{c.key}</span>
                  <Show when={c.value}>{(v) => <span class="rl-chip-v">{v()}</span>}</Show>
                  <button
                    type="button"
                    class="rl-chip-x"
                    aria-label={`Remove the ${c.key} filter`}
                    title="Remove"
                    onClick={() => filterSetter(c.param)("")}
                  >
                    <Icon icon="x" size={10} />
                  </button>
                </span>
              )}
            </For>
            <button type="button" class="rl-linkbtn" onClick={() => clearFilters()}>
              Clear all
            </button>
          </div>
        </Show>

        <Show when={failedAt()}>
          {(at) => (
            <div class="rl-errbar" role="alert">
              <Icon icon="alert-triangle" size={14} />
              <span>
                <Show when={rows() !== null} fallback="Could not load the Runs.">
                  Could not refresh the Runs. Showing what loaded at {clockTime(loadedAt() ?? at())}.
                </Show>
              </span>
              <span class="rl-spacer" />
              <button type="button" class="btn btn-ghost btn-sm" onClick={retry}>
                <Icon icon="rotate-cw" size={13} /> Try again
              </button>
            </div>
          )}
        </Show>

        <Switch>
          <Match when={loading()}>
            <div class="runlist" aria-busy="true" aria-label="Loading the Runs">
              <For each={[0, 1, 2, 3, 4, 5]}>{(n) => <SkeletonRow n={n} />}</For>
            </div>
          </Match>
          <Match when={rows() === null}>{null}</Match>
          <Match when={neverRan()}>
            <div class="runlist">
              <div class="rl-empty rl-never">
                <div class="empty-land">
                  <AsciiScene scene={ponderIdle} fontSize={4} label="Beetle dozing with its back against its dung ball" />
                </div>
                <p class="rl-empty-title">This repo has not run yet</p>
                <p class="rl-empty-text">
                  Scarab starts a Run when a pipeline file in <span class="rl-mono">.scarab/</span> matches a push, a
                  pull request or a schedule.
                </p>
                <pre class="rl-code">{FIRST_PIPELINE}</pre>
                <div class="rl-empty-acts">
                  <a class="btn btn-ghost" href={PIPELINE_GUIDE} target="_blank" rel="noreferrer">
                    Read the pipeline guide
                  </a>
                  <button class="btn btn-primary" onClick={() => nav(`/${org()}/${repo()}/run`)}>
                    <Icon icon="play" size={14} /> Run pipeline
                  </button>
                </div>
              </div>
            </div>
          </Match>
          <Match when={listRows().length === 0}>
            <div class="runlist">
              <div class="rl-empty">
                <p class="rl-empty-title">No Runs match these filters</p>
                <p class="rl-empty-text">
                  <For each={noMatchSentence(panelFilters(), segment(), shownRuns().length)}>
                    {(p) => (p.t === "mono" ? <span class="rl-mono">{p.text}</span> : p.text)}
                  </For>
                </p>
                <div class="rl-empty-acts">
                  <button type="button" class="btn btn-ghost" onClick={() => clearFilters(true)}>
                    Clear filters
                  </button>
                </div>
              </div>
            </div>
          </Match>
          <Match when={true}>
            <Show when={arrived() > 0}>
              <div class="rl-newpill">
                <button type="button" onClick={showArrived}>
                  <Icon icon="arrow-up" size={13} /> {arrivedLabel(arrived())}
                </button>
              </div>
            </Show>
            <div class="runlist">
              <For each={view().order}>
                {(k) => {
                  const entry = () => view().byKey.get(k);
                  return (
                    <Show when={entry()}>
                      {(e) => {
                        const ev = e();
                        if (ev.t === "day") {
                          const g = () => (entry() as Extract<Entry, { t: "day" }> | undefined)?.g ?? ev.g;
                          return <DayHeader label={g().label} count={g().count} failed={g().failed} />;
                        }
                        const item = () => (entry() as Extract<Entry, { t: "row" }> | undefined)?.item ?? ev.item;
                        return (
                          <Show
                            when={item().row.kind === "run"}
                            fallback={
                              <RefusedRow
                                r={(item().row as Extract<Row, { kind: "refused" }>).refused}
                                repoUrl={repoUrl()}
                              />
                            }
                          >
                            <RunListRow
                              r={(item().row as Extract<Row, { kind: "run" }>).run}
                              child={item().child}
                              replaced={item().replaced}
                              now={now()}
                              repoUrl={repoUrl()}
                              href={`/${org()}/${repo()}/runs/${(item().row as Extract<Row, { kind: "run" }>).run.id}`}
                            />
                          </Show>
                        );
                      }}
                    </Show>
                  );
                }}
              </For>
            </div>
            <p class="rl-foot">
              Showing the latest {shownRuns().length} {shownRuns().length === 1 ? "Run" : "Runs"}
            </p>
          </Match>
        </Switch>
      </Show>

      {/* ---- Schedules ---- */}
      <Show when={tab() === "schedules"}>
        <RepoSchedules org={org()} repo={repo()} />
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
        <RepoSettings org={org()} repo={repo()} project={project() ?? null} />
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
// One table, a row per Environment: the rules a deploy from this project
// meets (the server's `effective` block, never merged here), what this
// project adds, and a link to its runs there. A row expands to set the org
// baseline beside this project's overlay. The project can only tighten (its
// overlay); the baseline, who may deploy and any privileges are an org
// admin's, under Settings › Environments. Which rows are open lives in this
// component only, not the URL.

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
  const [open, setOpen] = createSignal<ReadonlySet<string>>(new Set());
  const toggle = (name: string) =>
    setOpen((prev) => {
      const next = new Set(prev);
      if (!next.delete(name)) next.add(name);
      return next;
    });

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
          <div class="panel envt-panel">
            <div class="envt-scroll">
              <table class="envt">
                <thead>
                  <tr>
                    <th scope="col">Environment</th>
                    <th scope="col">Approvers</th>
                    <th scope="col">Refs</th>
                    <th scope="col">Wait</th>
                    <th scope="col">This repo</th>
                    <th scope="col">Runs</th>
                  </tr>
                </thead>
                <tbody>
                  <For each={envs()}>
                    {(env) => (
                      <EnvRow
                        org={props.org}
                        repo={props.repo}
                        env={env}
                        expanded={open().has(env.name)}
                        onToggle={() => toggle(env.name)}
                        onTighten={() => props.onTighten(env.name)}
                        onChanged={props.onChanged}
                      />
                    )}
                  </For>
                </tbody>
              </table>
            </div>
          </div>
        </Show>
      </Show>
    </Show>
  );
}

// One Environment's row and, when expanded, its detail row under it.
function EnvRow(props: {
  org: string;
  repo: string;
  env: ProjectEnvironment;
  expanded: boolean;
  onToggle: () => void;
  onTighten: () => void;
  onChanged: () => void;
}) {
  const [removing, setRemoving] = createSignal(false);
  const detailId = () => `envt-detail-${props.env.name}`;
  const wait = () => waitCell(props.env.effective);
  const chip = () => repoChip(props.env);
  const onKey = (e: KeyboardEvent) => {
    // Only the row itself: Enter on the Runs link follows the link.
    if (e.target !== e.currentTarget) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      props.onToggle();
    }
  };

  return (
    <>
      <tr
        class="envt-row"
        tabindex="0"
        aria-expanded={props.expanded}
        aria-controls={props.expanded ? detailId() : undefined}
        onClick={props.onToggle}
        onKeyDown={onKey}
      >
        <td>
          <span class="envt-chev" aria-hidden="true">
            <Icon icon={props.expanded ? "chevron-down" : "chevron-right"} size={14} />
          </span>
          {/* The name once; the name pipelines use is in the detail and the
              tooltip. */}
          <span
            class="envt-name"
            classList={{ "envt-mono": nameBeside(props.env) === null }}
            title={nameBeside(props.env) ? `Pipelines name it ${props.env.name}` : undefined}
          >
            {shownName(props.env)}
          </span>
        </td>
        <td>{approversCell(props.env)}</td>
        <td class="envt-mono">{refsCell(props.env.effective.allowed_ref_layers)}</td>
        <td title={wait().title}>{wait().text}</td>
        <td>
          <Show when={chip()} fallback={<span class="envt-quiet">—</span>}>
            {(c) => (
              <span class="envt-repo" classList={{ warn: c().warn }}>
                <Show when={c().warn}>
                  <Icon icon="alert-triangle" size={13} />
                </Show>
                {c().text}
              </span>
            )}
          </Show>
        </td>
        <td>
          <A
            class="envt-link"
            href={`/${props.org}/${props.repo}?env=${encodeURIComponent(props.env.name)}`}
            aria-label={`Runs in ${shownName(props.env)}`}
            onClick={(e) => e.stopPropagation()}
          >
            View runs
          </A>
        </td>
      </tr>
      <Show when={props.expanded}>
        <tr class="envt-detail" id={detailId()}>
          <td colspan="6">
            <p class="envt-sentence">
              <For each={deploySentence(props.env)}>
                {(s) => (s.mono ? <span class="envt-mono">{s.text}</span> : s.text)}
              </For>
            </p>
            <Show when={nameBeside(props.env)}>
              <p class="envt-pname">
                Pipelines name it <span class="envt-mono">{props.env.name}</span>.
              </p>
            </Show>
            <Show when={!props.env.eligible}>
              <p class="envt-note">An org admin withdrew this repo. Its overlay stays until removed.</p>
            </Show>
            {/* Side by side, like a split diff: the org's rules on the left,
                the rules in force here on the right; a rule this repo
                changed is marked + and shaded. */}
            <div class="envt-diff-scroll">
              <table class="envt-diff">
                <thead>
                  <tr>
                    <th scope="col">
                      <span class="sr-only">Rule</span>
                    </th>
                    <th scope="col">Org</th>
                    <th scope="col">In force here</th>
                  </tr>
                </thead>
                <tbody>
                  <For each={ruleDiff(props.env)}>
                    {(r) => {
                      const mono = (v: string) => r.mono && v !== "none" && v !== "Any";
                      return (
                        <tr classList={{ changed: r.changed }}>
                          <th scope="row">{r.rule}</th>
                          <td classList={{ "envt-mono": mono(r.org) }}>{r.org}</td>
                          <td class="envt-diff-now">
                            <span class="envt-diff-mark" aria-hidden="true">
                              {r.changed ? "+" : ""}
                            </span>
                            <Show when={r.changed}>
                              <span class="sr-only">changed by this repo: </span>
                            </Show>
                            <span classList={{ "envt-mono": mono(r.now) }}>{r.now}</span>
                          </td>
                        </tr>
                      );
                    }}
                  </For>
                </tbody>
              </table>
            </div>
            <Show when={props.env.eligible || props.env.overlay}>
              <div class="envt-actions">
                <button class="btn btn-ghost envt-btn" type="button" onClick={props.onTighten}>
                  {props.env.overlay ? "Edit this repo's rules" : "Add rules for this repo"}
                </button>
                <Show when={props.env.overlay}>
                  <button class="btn btn-danger envt-btn" type="button" onClick={() => setRemoving(true)}>
                    Remove
                  </button>
                </Show>
              </div>
            </Show>
          </td>
        </tr>
      </Show>
      <Show when={removing()}>
        <ConfirmDestructive
          eyebrow={`${props.org}/${props.repo} · environments`}
          title={`Remove ${props.repo}'s rules on ${props.env.name}?`}
          purpose="Its deploys there fall back to the org baseline."
          name={props.env.name}
          confirmLabel="Remove rules"
          onConfirm={async () => {
            await deleteEnvironmentOverlay(props.org, props.env.name, props.repo);
            props.onChanged();
          }}
          onClose={() => setRemoving(false)}
        >
          <Show when={props.env.overlay}>
            {(o) => (
              <p class="dlg-ctx">
                <span class="k">This repo adds</span> {rulesSummary(o())}
              </p>
            )}
          </Show>
        </ConfirmDestructive>
      </Show>
    </>
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
                <span class="k">Org baseline</span>{" "}
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

// ---- schedules (the Project's `on: cron:` triggers) -----------------------
// One table, a row per schedule: the pipeline, its expression (UTC, as read
// from the default branch), the next slot, how the last slot came out, and a
// link to the Runs tab showing that pipeline's scheduled Runs.

function RepoSchedules(props: { org: string; repo: string }) {
  // An older server has no schedules endpoint; it reads as none.
  const [schedules] = createResource(
    () => ({ org: props.org, repo: props.repo }),
    (k) => listSchedules(k.org, k.repo).catch(() => [] as Schedule[]),
  );
  const rows = () => schedules() ?? [];
  return (
    <Show when={!schedules.loading} fallback={<p class="empty">loading…</p>}>
      <Show
        when={rows().length > 0}
        fallback={
          <p class="empty">
            No schedules. Add <span class="mono">on: cron:</span> to a pipeline on the default
            branch and its schedule shows here.
          </p>
        }
      >
        <div class="panel">
          <div class="panel-h">
            <span>
              Schedules <span class="tok-count">{rows().length}</span>
            </span>
          </div>
          <div class="secrets-body">
            <div class="table-scroll">
              <table class="table lib-table sched-table" aria-label="Schedules">
                <thead>
                  <tr>
                    <th>Pipeline</th>
                    <th>Schedule</th>
                    <th>Next</th>
                    <th>Last run</th>
                    <th>Outcome</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  <For each={rows()}>
                    {(s) => <ScheduleRow org={props.org} repo={props.repo} s={s} />}
                  </For>
                </tbody>
              </table>
            </div>
          </div>
        </div>
      </Show>
    </Show>
  );
}

/** One `on: cron:` schedule as a table row. */
function ScheduleRow(props: { org: string; repo: string; s: Schedule }) {
  const s = () => props.s;
  return (
    <tr>
      <td title={s().pipeline}>
        <span class="mono">{pipelineShortName(s().pipeline)}</span>
      </td>
      <td class="nowrap" title={`read from ${s().source_sha.slice(0, 7)} on the default branch`}>
        <span class="mono">{s().schedule}</span> UTC
      </td>
      <td class="nowrap" title={absTime(s().next_fire_at)}>
        {untilTime(s().next_fire_at)}
      </td>
      <Show when={s().last_fire} fallback={<td class="subtle">not fired yet</td>}>
        {(f) => (
          <td class="nowrap" title={absTime(f().slot_at)}>
            <Show when={f().run_id} fallback={relTime(f().slot_at)}>
              {(id) => (
                <A class="oenv-link" href={`/${props.org}/${props.repo}/runs/${id()}`}>
                  {relTime(f().slot_at)}
                </A>
              )}
            </Show>
            <Show when={f().schedule !== s().schedule}>
              {" "}
              <span class="mono" title="the schedule that fired">
                {f().schedule}
              </span>
            </Show>
          </td>
        )}
      </Show>
      <td class="nowrap">
        <Show when={s().last_fire}>
          {(f) => <span class={`rr-status ${fireOutcome(f().outcome).tone}`}>{fireOutcome(f().outcome).label}</span>}
        </Show>
      </td>
      <td class="acts keep">
        <A
          class="btn btn-ghost btn-sm"
          href={scheduledRunsHref(props.org, props.repo, s().pipeline)}
          title={`the scheduled runs of ${pipelineShortName(s().pipeline)}`}
        >
          Runs
        </A>
      </td>
    </tr>
  );
}

// ---- the runs list ---------------------------------------------------------

/** A day header: `Today  9 runs · 3 failed or refused`. */
function DayHeader(props: { label: string; count: number; failed: number }) {
  const sum = () => daySummary(props);
  return (
    <div class="rr-day">
      {props.label}
      <span class="sum">
        {sum().runs}
        <Show when={sum().failed}>
          {(f) => (
            <>
              {" · "}
              <span class="failed">{f()}</span>
            </>
          )}
        </Show>
      </span>
    </div>
  );
}

/** One Run's row. Its props are read where they are drawn, so a poll that
 *  changes a Run updates this row in place. */
function RunListRow(props: {
  r: RunSummary;
  child: boolean;
  replaced: Replaced | null;
  now: number;
  repoUrl?: string | null;
  href: string;
}) {
  const nav = useNavigate();
  const r = () => props.r;
  const ok = () => r().status === "succeeded" && !props.replaced;
  const st = () => rowStatus(r(), props.replaced);
  return (
    <div
      class="runrow"
      classList={{ ok: ok(), child: props.child, deploy: r().deploy }}
      onClick={() => nav(props.href)}
    >
      <StatusMark status={r().status} />
      <div class="rr-num">
        <RunNumber n={r().run_number} id={r().id} />
      </div>
      <div class="rr-main">
        <RowTitle pipeline={r().pipeline} kind={r().trigger_kind} title={r().trigger_title} />
        <OriginLine
          kind={r().trigger_kind}
          line={kindFirst({
            kind: r().trigger_kind,
            ref: r().git_ref,
            pr: r().pr_number,
            // A server that sends a pull request's head branch names it here.
            head: (r() as RunSummary & { origin_pr_head?: string | null }).origin_pr_head,
            base: r().origin_pr_base,
            title: r().trigger_title,
          })}
          sha={r().sha}
          author={authorSlot(r().trigger_kind, r().actor)}
          commitUrl={forgeCommitUrl(props.repoUrl, r().sha)}
          prUrl={forgePrUrl(props.repoUrl, r().pr_number)}
          deploy={deployTag(r().deploy, r().environment)}
        />
      </div>
      <RowStatus
        word={st().word}
        tone={st().tone}
        detail={st().detail}
        quiet={ok()}
        clock={clockTime(r().created_at)}
        when={`${relTime(r().created_at)} · ${absTime(r().created_at)}`}
        duration={durationLabel(r(), props.now)}
        live={isLive(r().status)}
      />
    </div>
  );
}

/** One picker of the Filter panel. */
type PanelPick = { value: string; set: (v: string) => void; options: SelectOption[] };

/** The Filter button and its panel: the pickers for pipeline, Environment,
 *  author, branch and PR, and the Deploys only and Scheduled only switches.
 *  A popover under the button; a sheet at the bottom of the screen on a
 *  phone. Closes on Escape (focus returns to the button) and on a click
 *  outside it. A picker's list is portalled to <body>, so a click in it is
 *  not outside. */
function FilterPanel(props: {
  count: number;
  pipeline: PanelPick;
  environment: PanelPick;
  author: PanelPick;
  branch: PanelPick;
  pr: PanelPick;
  deploys: boolean;
  onDeploys: (on: boolean) => void;
  scheduled: boolean;
  onScheduled: (on: boolean) => void;
}) {
  const [open, setOpen] = createSignal(false);
  const id = createUniqueId();
  let button: HTMLButtonElement | undefined;
  let panel: HTMLDivElement | undefined;
  const close = (refocus: boolean) => {
    setOpen(false);
    if (refocus) button?.focus();
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape" && open()) close(true);
  };
  const onDown = (e: PointerEvent) => {
    const t = e.target as Node | null;
    if (!open() || !t) return;
    if (panel?.contains(t) || button?.contains(t)) return;
    if (t instanceof Element && t.closest(".ss-pop")) return;
    close(false);
  };
  document.addEventListener("keydown", onKey);
  document.addEventListener("pointerdown", onDown);
  onCleanup(() => {
    document.removeEventListener("keydown", onKey);
    document.removeEventListener("pointerdown", onDown);
  });
  const toggle = () => {
    if (open()) return close(false);
    setOpen(true);
    queueMicrotask(() => panel?.querySelector<HTMLElement>("button, [tabindex]")?.focus());
  };
  // Label, icon, placeholder, the picker, and whether its values are
  // machine values (set in Mono).
  const picks: [string, string, string, () => PanelPick, boolean][] = [
    ["Pipeline", "workflow", "any pipeline", () => props.pipeline, true],
    ["Environment", "shield-check", "any environment", () => props.environment, false],
    ["Author", "user", "any author", () => props.author, true],
    ["Branch", "git-branch", "any branch", () => props.branch, true],
    ["Pull request", "git-pull-request", "any PR", () => props.pr, false],
  ];
  return (
    <span class="rl-pop">
      <button
        type="button"
        class="rl-fbtn"
        classList={{ on: open() }}
        ref={button}
        aria-expanded={open()}
        aria-controls={id}
        onClick={toggle}
      >
        <Icon icon="filter" size={13} /> Filter
        <Show when={props.count > 0}>
          <span class="rl-badge" aria-label={`${props.count} on`}>
            {props.count}
          </span>
        </Show>
      </button>
      <Show when={open()}>
        <div class="rl-sheet-backdrop" aria-hidden="true" />
        <div class="rl-panel" id={id} role="dialog" aria-label="Filter the Runs" ref={panel}>
          <For each={picks}>
            {([label, icon, placeholder, pick, mono]) => (
              <Show when={pick().options.length > 0 || pick().value}>
                <div class="rl-prow">
                  <span class="rl-pk">
                    <Icon icon={icon} size={13} /> {label}
                  </span>
                  <SearchSelect
                    class="ss-block"
                    mono={mono}
                    clearable
                    placeholder={placeholder}
                    searchPlaceholder={`Search ${label.toLowerCase()}s…`}
                    aria-label={label}
                    value={pick().value}
                    onChange={pick().set}
                    options={pick().options}
                  />
                </div>
              </Show>
            )}
          </For>
          <div class="rl-switches">
            <button
              type="button"
              role="switch"
              class="rl-switch"
              aria-checked={props.deploys}
              title="only deploy runs: a pipeline or step marked `deploy: true`"
              onClick={() => props.onDeploys(!props.deploys)}
            >
              <span class="rl-track" aria-hidden="true" />
              Deploys only
            </button>
            <button
              type="button"
              role="switch"
              class="rl-switch"
              aria-checked={props.scheduled}
              title="only runs a schedule started: a pipeline's `on: cron:`"
              onClick={() => props.onScheduled(!props.scheduled)}
            >
              <span class="rl-track" aria-hidden="true" />
              Scheduled only
            </button>
          </div>
        </div>
      </Show>
    </span>
  );
}

/** A trigger that did not become a Run: where a Run would be, with the
 *  reason and its fix on one line under it. */
function RefusedRow(props: { r: RefusedRun; repoUrl?: string | null }) {
  const r = () => props.r;
  const sum = () => refusalSummary(r().errors, r().message);
  const [all, setAll] = createSignal(false);
  const pr = () => pullNumber(r().git_ref);
  const fixes = () =>
    [sum().fix, r().reason_kind === "yanked" ? "an org admin upgrades the installation, then push again" : null].filter(
      (f): f is string => !!f,
    );
  return (
    <div class="runrow refused">
      <StatusMark status="refused" />
      <div class="rr-num">
        <span class="rr-nonum" title="no Run: the trigger was refused">
          –
        </span>
      </div>
      <div class="rr-main">
        <RowTitle pipeline={pipelineShortName(r().pipeline)} kind={r().trigger_kind} title={r().title} />
        <OriginLine
          kind={r().trigger_kind}
          line={kindFirst({ kind: r().trigger_kind, ref: r().git_ref, title: r().title })}
          sha={r().sha}
          author={authorSlot(r().trigger_kind, r().actor)}
          commitUrl={forgeCommitUrl(props.repoUrl, r().sha)}
          prUrl={forgePrUrl(props.repoUrl, pr())}
        />
      </div>
      <RowStatus
        word={statusWord("refused")}
        tone="refused"
        clock={clockTime(r().created_at)}
        when={`${relTime(r().created_at)} · ${absTime(r().created_at)}`}
      />
      <div class="rr-reason">
        <Icon icon={r().reason_kind === "compile" ? "circle-x" : "ban"} size={13} />
        <span>
          {/* A message may hold a YAML snippet to paste: keep its lines. */}
          <span class="what">{sum().what}</span>
          <For each={fixes()}>{(fix) => <span class="fix"> · {fix}</span>}</For>
          <Show when={sum().more > 0}>
            <span class="fix"> · </span>
            <button class="xs-link" onClick={() => setAll((v) => !v)}>
              {all() ? "hide the other errors" : `${sum().more} more ${sum().more === 1 ? "error" : "errors"}`}
            </button>
          </Show>
        </span>
      </div>
      <Show when={all()}>
        <For each={r().errors.slice(1)}>
          {(e) => (
            <div class="rr-reason">
              <Icon icon="circle-x" size={13} />
              <span class="what">{refusalSummary([e], e.message).what}</span>
            </div>
          )}
        </For>
      </Show>
    </div>
  );
}
