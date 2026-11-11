// Dashboard — the engineer's landing tier, driven by the real API:
// (1) an ACTION INBOX of suspended runs (a gate is holding them until someone
// decides); (2) RECENTLY VISITED repos (browser-local) as status
// cards, each with a rounded pass/fail bar strip of its last runs; (3) the full
// repo list, most-recently-active first, with a search for orgs with many repos.
// The old flat activity table is gone — recency now lives on the repo cards.
import { For, Show, createMemo, createResource, createSignal } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import {
  listProjects,
  listRuns,
  listRepoRuns,
  type Project,
  type RunSummary,
} from "../api/client";
import { getVisited } from "../visited";
import { me } from "../session";
import { addConnectionHref } from "../settings-nav";
import { orgPageHref } from "../org-page";
import { inboxDetail, inboxRuns, inboxVerb } from "../gates";
import { relTime, absTime } from "../fmt";
import Icon from "../components/Icon";
import RunBars from "../components/RunBars";
import AsciiScene from "../components/AsciiScene";
import dungroller from "../../../brand/ascii/generated/dungroller-bare.json";
import Pill from "../components/Pill";
import { HomeCrumb } from "../components/PageCrumbs";

const runHref = (r: RunSummary) => `/${r.org ?? "api"}/${r.project ?? "unknown"}/runs/${r.id}`;
const tenant = (r: RunSummary) => (r.org && r.project ? `${r.org}/${r.project}` : null);
const shortId = (id: string) => id.slice(0, 8);

// The empty repo list: where repositories come from. An org admin enables
// them on the org's page; with no org yet, a global admin connects a forge on
// New organization (the one place a first connection is added). So this links
// there and offers no form of its own.
function NoRepos() {
  const orgs = () => (me()?.can_administer ? me()!.admin_orgs : []);
  const connect = () => (me() ? addConnectionHref(me()!) : null);
  return (
    <>
      No repositories enabled yet.{" "}
      <Show
        when={orgs().length > 0}
        fallback={
          <Show
            when={connect()}
            fallback="An organization admin enables them on the organization's page."
          >
            {(h) => (
              <>
                <A href={h()}>Connect a forge</A> to bring them in.
              </>
            )}
          </Show>
        }
      >
        Enable them on the page of{" "}
        <For each={orgs()}>
          {(o, i) => (
            <>
              {i() > 0 ? ", " : ""}
              <A href={orgPageHref(o)} class="mono">
                {o}
              </A>
            </>
          )}
        </For>
        .
      </Show>
    </>
  );
}

// One "recently visited" status card: the repo's last runs as a pass/fail strip.
function RepoStatusCard(props: { p: Project }) {
  const [runs] = createResource(
    () => props.p,
    (p) => listRepoRuns(p.org, p.project, 14),
  );
  const settled = () =>
    (runs() ?? []).filter(
      (r) => r.status === "succeeded" || r.status === "failed" || r.status === "cancelled",
    );
  const passRate = () => {
    const s = settled();
    if (!s.length) return null;
    return Math.round((s.filter((r) => r.status === "succeeded").length / s.length) * 100);
  };
  return (
    <A href={`/${props.p.org}/${props.p.project}`} class="rcard">
      <div class="rcard-head">
        <span class="repo-name">
          <span class="repo-org">{props.p.org} /</span> {props.p.project}
        </span>
        <Icon icon="chevron-right" size={15} class="repo-go" />
      </div>
      <RunBars runs={runs() ?? []} />
      <div class="rcard-foot mono">
        <Show when={passRate() !== null} fallback={<span class="subtle">no runs yet</span>}>
          <span class={passRate()! >= 90 ? "ok" : passRate()! >= 60 ? "warn" : "fail"}>
            {passRate()}% pass
          </span>
        </Show>
        <Show when={props.p.last_run_at}>
          <span class="dotsep">·</span>
          <span class="subtle" title={absTime(props.p.last_run_at!)}>
            {relTime(props.p.last_run_at!)}
          </span>
        </Show>
      </div>
    </A>
  );
}

export default function Repos() {
  const nav = useNavigate();
  const [projects] = createResource(listProjects);
  const [runs] = createResource(() => listRuns(50));
  const [query, setQuery] = createSignal("");

  // Runs waiting on a gate — by their reached gates, not run status: a gate
  // reached while a sibling still runs leaves the run `running` (c489346).
  const inbox = () => inboxRuns(runs() ?? []);

  // Recently visited (browser-local); falls back to the most-active repos so a
  // fresh browser is never empty. Resolved against the project list for metadata.
  const visited = createMemo<Project[]>(() => {
    const projs = projects() ?? [];
    const byKey = new Map(projs.map((p) => [`${p.org}/${p.project}`, p]));
    const local = getVisited()
      .map((v) => byKey.get(`${v.org}/${v.project}`))
      .filter((p): p is Project => Boolean(p));
    return (local.length ? local : projs).slice(0, 4);
  });
  const usingFallback = () => getVisited().length === 0;

  // The full list, most-recently-active first (server-sorted), client-filtered.
  const filtered = createMemo<Project[]>(() => {
    const q = query().trim().toLowerCase();
    const projs = projects() ?? [];
    if (!q) return projs;
    return projs.filter((p) =>
      `${p.org}/${p.project} ${p.owner}/${p.name}`.toLowerCase().includes(q),
    );
  });

  const loading = () => projects.loading || runs.loading;

  return (
    <>
      <section class="page">
        <div class="page-head">
          <h1 class="crumb-head">
            <HomeCrumb size={20} sepClass="crumb-head-sep" />
            <span>Dashboard</span>
          </h1>
        </div>
        {/* Empty, like a repository's when it has no action: the row keeps the
            first block at the height of the tabs on the other pages. */}
        <div class="page-toolbar" />

        <Show when={runs.error || projects.error}>
          <p class="error">
            {runs.error ? "Could not load runs." : "Could not load repositories."} Is the server up?
          </p>
        </Show>

        <Show when={!loading()} fallback={<p class="empty">loading…</p>}>
          {/* ── Waiting on you (action inbox: runs with a waiting gate) ───────── */}
          <Show
            when={inbox().length > 0}
            fallback={
              <div class="allclear">
                <Icon icon="shield-check" size={22} class="ac-ico" />
                <div>
                  <div class="ac-title">All clear — nothing needs you</div>
                  <div class="ac-sub">No run is waiting on a gate. New work will surface here.</div>
                </div>
              </div>
            }
          >
            <div class="dash-label">
              Waiting on you <span class="count">· {inbox().length}</span>
            </div>
            <div class="inbox">
              <For each={inbox()}>
                {(r) => (
                  <div class="inbox-row" onClick={() => nav(runHref(r))}>
                    <Pill tone="ok" class="inbox-verb">{inboxVerb(r)}</Pill>
                    <span class="inbox-what">
                      <span class="inbox-msg">
                        {inboxVerb(r)} gate — run <span class="sha mono">{shortId(r.id)}</span>
                        <Show when={tenant(r)}>{(t) => <span class="repo"> {t()}</span>}</Show>
                      </span>
                      <span class="inbox-detail">{inboxDetail(r)}</span>
                    </span>
                    <span class="inbox-age" title={absTime(r.created_at)}>
                      {relTime(r.created_at)}
                    </span>
                    <button
                      class="btn btn-sm btn-primary"
                      onClick={(e) => {
                        e.stopPropagation();
                        nav(runHref(r));
                      }}
                    >
                      Review
                    </button>
                  </div>
                )}
              </For>
            </div>
          </Show>

          {/* ── Recently visited (status cards with pass/fail strip) ──────────── */}
          <Show when={visited().length > 0}>
            <div class="dash-label">
              {usingFallback() ? "Most active" : "Recently visited"}
              <span class="count">· {visited().length}</span>
            </div>
            <div class="rcard-grid">
              <For each={visited()}>{(p) => <RepoStatusCard p={p} />}</For>
            </div>
          </Show>

          {/* ── All repos (recency-sorted, searchable) ───────────────────────── */}
          <div class="dash-label repos-label">
            Repos <span class="count">· {projects()?.length ?? 0}</span>
            <Show when={(projects()?.length ?? 0) > 6}>
              <div class="repo-search">
                <Icon icon="search" size={13} />
                <input
                  type="text"
                  placeholder="Filter repos…"
                  value={query()}
                  onInput={(e) => setQuery(e.currentTarget.value)}
                />
              </div>
            </Show>
          </div>
          <Show
            when={filtered().length > 0}
            fallback={<p class="empty">{query() ? "No repos match." : <NoRepos />}</p>}
          >
            <div class="repo-list">
              <For each={filtered()}>
                {(p) => (
                  <A href={`/${p.org}/${p.project}`} class="repo-row">
                    <Icon icon="git-branch" size={14} class="rr-ico" />
                    <span class="repo-name">
                      <span class="repo-org">{p.org} /</span> {p.project}
                    </span>
                    {/* The forge repo the project is bound to; only worth a
                        second line when it is not named like the project. */}
                    <Show
                      when={
                        `${p.owner}/${p.name}`.toLowerCase() !==
                        `${p.org}/${p.project}`.toLowerCase()
                      }
                    >
                      <span class="rr-coord mono">
                        {p.owner}/{p.name}
                      </span>
                    </Show>
                    <span class="rr-when mono">
                      {p.last_run_at ? relTime(p.last_run_at) : "never run"}
                    </span>
                    <Icon icon="chevron-right" size={15} class="repo-go" />
                  </A>
                )}
              </For>
            </div>
          </Show>
        </Show>
      </section>

      <footer class="dash-footer rolling" aria-hidden="true">
        <div class="roller">
          <AsciiScene scene={dungroller} fontSize={3.33} playing={true} />
        </div>
      </footer>
    </>
  );
}
