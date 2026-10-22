// Deployments (ADR-0070) — what is live on each DeploymentTarget, and how it
// got there. Org-scoped, not per repo: an Environment's targets are fed by many
// Projects, and "what is in production" is one question.
//
// Two panels: the **Environment board** (per target: what is live, which
// Deployment made it live, and anything newer still in flight) and, below it,
// one Environment's **Deployments**, newest first, each expandable into the
// stages its source reported. The Org and the Environment are URL state
// (`?org=`, `?env=`), so a link from a repo's Environments tab lands on the
// right board even for a user in several orgs; absent, the org falls back to
// one the caller administers or belongs to, and the first Environment on the
// board is shown.
//
// The Environment pills are the org's registered Environments (ADR-0071)
// united with every name the record carries, so a registered Environment with
// nothing deployed yet is still pickable, and a name with no org Environment
// is marked: its Deployments stall by the default deadline. A caller who
// cannot read the org's Environments still gets the record's names, unmarked.
//
// Everything here is the server's record, shown — the status, Unexplained and
// Unconfirmed are core's verdicts (CONTEXT.md §4.6), not re-derived.
import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import { A, useSearchParams } from "@solidjs/router";
import {
  DeploymentsForbiddenError,
  getEnvironmentBoard,
  listDeploymentTargets,
  listEnvironmentDeployments,
  listOrgEnvironments,
  listProjects,
  type Deployment,
  type DeploymentRequester,
  type Project,
} from "../api/client";
import { me } from "../session";
import { relTime, absTime } from "../fmt";
import {
  deploymentTags,
  groupBoard,
  inFlight,
  madeLiveBy,
  pickOrg,
  requesterHref,
  safeHref,
  targetLabel,
} from "../deployments";
import { envPills } from "../org-environments";
import { movedAwayNote, targetKey, targetOwners } from "../plugins";
import StatusBadge from "../components/StatusBadge";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";

const one = (v: string | string[] | undefined): string => (Array.isArray(v) ? v[0] : v) ?? "";

export default function Deployments() {
  const [search, setSearch] = useSearchParams();
  const [projects] = createResource(() => listProjects().catch(() => [] as Project[]));
  // `?org=` decides at once. Otherwise wait for the identity before falling
  // back to a project's org, so the page never reads one org and then flips
  // to another.
  const org = createMemo(() =>
    one(search.org)
      ? one(search.org)
      : me.loading || projects.loading
        ? undefined
        : pickOrg(undefined, me()?.admin_orgs, (projects() ?? []).map((p) => p.org)),
  );

  const [board] = createResource(org, (o) => getEnvironmentBoard(o));
  const groups = createMemo(() => groupBoard(board() ?? []));

  // Each target's owners, as its source last reported them (ADR-0072). Best
  // effort: a 403/404 (or any failure) just leaves the owners line off.
  const [owners] = createResource(
    () => {
      const envs = groups().map((g) => g.environment);
      return org() && envs.length > 0 ? { org: org()!, envs } : false;
    },
    async (k) => {
      const lists = await Promise.all(
        k.envs.map((e) => listDeploymentTargets(k.org, e).catch(() => null)),
      );
      return new Map(k.envs.map((e, i) => [e, targetOwners(lists[i] ?? [])]));
    },
  );
  const ownersOf = (env: string, source: string, id: string) =>
    owners()?.get(env)?.get(targetKey(source, id));

  /** The Environment whose Deployments are listed: `?env=`, else the first
   *  on the board, else the first registered one. */
  const env = () => one(search.env) || groups()[0]?.environment || pills()[0]?.name || "";
  // `null` (not wired, or no access to them) leaves the pills unmarked.
  const [orgEnvs] = createResource(org, (o) => listOrgEnvironments(o).catch(() => null));
  const pills = createMemo(() =>
    envPills(
      orgEnvs.loading ? null : orgEnvs(),
      groups().map((g) => g.environment),
      one(search.env) || undefined,
    ),
  );
  const pickEnv = (e: string) => setSearch({ env: e });

  const [list] = createResource(
    () => (org() && env() ? { org: org()!, env: env() } : false),
    (k) => listEnvironmentDeployments(k.org, k.env),
  );

  return (
    <section class="page">
      <Doodle icon="rocket" size={220} rotate={-8} opacity={0.16} top="52px" right="48px" />

      <div class="page-head">
        <h1>Deployments</h1>
        <Show when={org()}>
          {(o) => (
            <span class="mono subtle" title="The organization whose Environments this page shows">
              {o()}
            </span>
          )}
        </Show>
      </div>
      <p class="page-sub">what is live on each target, and how it got there</p>

      <Show
        when={org()}
        fallback={
          <Show when={!me.loading && !projects.loading} fallback={<p class="empty">loading…</p>}>
            <p class="empty">
              No organization yet. Deployments appear once a repository is connected and deploys.
            </p>
          </Show>
        }
      >
        {(o) => (
          <Show when={!board.loading} fallback={<p class="empty">loading…</p>}>
            <Show
              when={!board.error}
              fallback={
                board.error instanceof DeploymentsForbiddenError ? (
                  <p class="empty">
                    You cannot read <span class="mono">{o()}</span>'s deployments. Reading them needs access
                    to the whole organization; access to a single repository is not enough yet.
                  </p>
                ) : (
                  <p class="error">Could not load the environment board.</p>
                )
              }
            >
              <Show
                when={board() !== null}
                fallback={<p class="empty">This server does not record deployments.</p>}
              >
                <Show
                  when={groups().length > 0 || pills().length > 0}
                  fallback={<p class="empty">Nothing has been deployed yet.</p>}
                >
                  <For each={groups()}>
                    {(g) => (
                      <div class="panel">
                        <div class="panel-h">
                          <span>
                            <Icon icon="rocket" size={12} /> {g.environment}
                          </span>
                          <span class="mono">
                            {g.rows.length} target{g.rows.length === 1 ? "" : "s"}
                          </span>
                        </div>
                        <table class="table dep-table">
                          <thead>
                            <tr>
                              <th>Target</th>
                              <th>Live</th>
                              <th>Made live by</th>
                              <th>In flight</th>
                            </tr>
                          </thead>
                          <tbody>
                            <For each={g.rows}>
                              {(row) => (
                                <tr
                                  onClick={() => pickEnv(g.environment)}
                                  title={`Show ${g.environment}'s deployments`}
                                >
                                  <td>
                                    <span class="mono dep-target">{targetLabel(row.target)}</span>
                                    <Show
                                      when={ownersOf(g.environment, row.target.source, row.target.id)}
                                    >
                                      {(o) => (
                                        <div
                                          class="dep-owners mono"
                                          title="The projects that own this target, as its source last reported"
                                        >
                                          owners: {o().join(", ")}
                                        </div>
                                      )}
                                    </Show>
                                  </td>
                                  <td>
                                    {/* Moved (ADR-0070 §3): its source routes it elsewhere
                                        now, so what was live here is history, not this
                                        Environment's state — shown as the note, with the
                                        last version here muted. */}
                                    <Show
                                      when={row.moved_away}
                                      fallback={
                                        <Show
                                          when={row.live}
                                          fallback={<span class="subtle">nothing live</span>}
                                        >
                                          {(l) => <span class="mono dep-version">{l().version}</span>}
                                        </Show>
                                      }
                                    >
                                      {(m) => (
                                        <span class="dep-cell" title={movedAwayNote(m()).title}>
                                          <span class="dep-tag dep-tag-muted">{movedAwayNote(m()).label}</span>
                                          <Show when={row.live}>
                                            {(l) => (
                                              <span class="mono subtle">last here: {l().version}</span>
                                            )}
                                          </Show>
                                        </span>
                                      )}
                                    </Show>
                                  </td>
                                  <td>
                                    <Show
                                      when={!row.moved_away && row.live}
                                      fallback={<span class="subtle">—</span>}
                                    >
                                      {(l) => (
                                        <span class="dep-cell">
                                          <span class="mono subtle">#{l().id}</span>
                                          <Requesters org={o()} requesters={madeLiveBy(l())} />
                                          <Tags d={l()} />
                                        </span>
                                      )}
                                    </Show>
                                  </td>
                                  <td>
                                    <Show when={inFlight(row)} fallback={<span class="subtle">—</span>}>
                                      {(n) => (
                                        <span class="dep-cell">
                                          <span class="mono subtle">#{n().id}</span>
                                          <span class="mono dep-version">{n().version}</span>
                                          <StatusBadge status={n().status} />
                                          <Tags d={n()} />
                                        </span>
                                      )}
                                    </Show>
                                  </td>
                                </tr>
                              )}
                            </For>
                          </tbody>
                        </table>
                      </div>
                    )}
                  </For>

                  <div class="filters dep-envs">
                    <For each={pills()}>
                      {(p) => (
                        <button
                          class="fpill"
                          classList={{ on: env() === p.name, "oenv-unregistered": p.registered === false }}
                          title={p.title}
                          onClick={() => pickEnv(p.name)}
                        >
                          {p.name}
                        </button>
                      )}
                    </For>
                  </div>

                  <div class="panel">
                    <div class="panel-h">
                      <span>Deployments · {env()}</span>
                      <Show when={list()}>
                        {(l) => <span class="mono">{l().length}</span>}
                      </Show>
                    </div>
                    <Show when={!list.loading} fallback={<p class="empty dep-pad">loading…</p>}>
                      <Show
                        when={!list.error}
                        fallback={<p class="error dep-pad">Could not load {env()}'s deployments.</p>}
                      >
                        <Show
                          when={(list() ?? []).length > 0}
                          fallback={<p class="empty dep-pad">No deployments in {env()}.</p>}
                        >
                          <DeploymentTable org={o()} rows={list()!} />
                        </Show>
                      </Show>
                    </Show>
                  </div>
                </Show>
              </Show>
            </Show>
          </Show>
        )}
      </Show>
    </section>
  );
}

/** One Environment's Deployments; a row click unfolds its reported stages. */
function DeploymentTable(props: { org: string; rows: Deployment[] }) {
  const [open, setOpen] = createSignal<ReadonlySet<number>>(new Set());
  const toggle = (id: number) =>
    setOpen((s) => {
      const next = new Set(s);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  return (
    <table class="table dep-table">
      <thead>
        <tr>
          <th class="dep-chev" />
          <th>#</th>
          <th>Target</th>
          <th>Version</th>
          <th>Status</th>
          <th>Requested by</th>
          <th>Created</th>
        </tr>
      </thead>
      <tbody>
        <For each={props.rows}>
          {(d) => (
            <>
              <tr onClick={() => toggle(d.id)} classList={{ "dep-open": open().has(d.id) }}>
                <td class="dep-chev">
                  <Icon icon={open().has(d.id) ? "chevron-down" : "chevron-right"} size={13} />
                </td>
                <td class="mono subtle">#{d.id}</td>
                <td class="mono dep-target">{targetLabel(d.target)}</td>
                <td class="mono dep-version">{d.version}</td>
                <td>
                  <span class="dep-cell">
                    <StatusBadge status={d.status} />
                    <Tags d={d} />
                  </span>
                </td>
                <td>
                  <Requesters org={props.org} requesters={d.requesters} />
                </td>
                <td class="mono subtle" title={absTime(d.created_at)}>
                  {relTime(d.created_at)}
                </td>
              </tr>
              <Show when={open().has(d.id)}>
                <tr class="dep-reports-row">
                  <td colSpan={7}>
                    <Reports d={d} />
                  </td>
                </tr>
              </Show>
            </>
          )}
        </For>
      </tbody>
    </table>
  );
}

/** The stages a Deployment's source reported, oldest first, as the record
 *  holds them. `at` is when Scarab received each; the source's own clock is on
 *  hover only (display-only, never used for ordering). */
function Reports(props: { d: Deployment }) {
  return (
    <div class="dep-reports">
      <For
        each={props.d.reports}
        fallback={
          <div class="dep-report subtle">
            no reports from <span class="mono">{props.d.target.source}</span> yet
          </div>
        }
      >
        {(r) => (
          <div class="dep-report">
            <StatusBadge status={r.status} />
            <span class="dep-label">{r.label}</span>
            <Show when={safeHref(r.link)}>
              {(href) => (
                <a class="dep-link mono" href={href()} target="_blank" rel="noopener noreferrer">
                  open ↗
                </a>
              )}
            </Show>
            <span
              class="mono subtle dep-at"
              title={`received ${absTime(r.at)}${r.source_at != null ? ` · source says ${absTime(r.source_at)}` : ""}`}
            >
              {relTime(r.at)}
            </span>
          </div>
        )}
      </For>
      <Show when={props.d.superseded_at}>
        {(at) => (
          <div class="dep-report subtle">
            <Icon icon="history" size={12} /> superseded{" "}
            <span class="mono" title={absTime(at())}>
              {relTime(at())}
            </span>
            {props.d.was_ever_live ? " — after being live" : " — before it ever went live"}
          </div>
        )}
      </Show>
    </div>
  );
}

/** Deploy Run steps that asked for a Deployment, linked to their run when the
 *  run's Project is known; a claim is labelled as one. The caller picks which:
 *  the board's "Made live by" passes only the non-claims. */
function Requesters(props: { org: string; requesters: DeploymentRequester[] }) {
  const stop = (e: MouseEvent) => e.stopPropagation();
  return (
    <Show when={props.requesters.length > 0} fallback={<span class="subtle">—</span>}>
      <span class="dep-cell">
        <For each={props.requesters}>
          {(r) => {
            const href = requesterHref(props.org, r);
            const text = `${r.run.slice(0, 8)} · ${r.step}`;
            const run = href ? (
              <A class="dep-run mono" href={href} onClick={stop} title={`run ${r.run} in ${r.project}`}>
                {text}
              </A>
            ) : (
              <span class="dep-run mono" title={`run ${r.run} (project unknown)`}>
                {text}
              </span>
            );
            return r.claim ? (
              <span class="dep-cell">
                {run}
                <span
                  class="dep-tag dep-tag-muted"
                  title="Only a claim: this run named the version after its source had already shown it, so it did not make the change."
                >
                  claim
                </span>
              </span>
            ) : (
              run
            );
          }}
        </For>
      </span>
    </Show>
  );
}

function Tags(props: { d: Deployment }) {
  return (
    <For each={deploymentTags(props.d)}>
      {(t) => (
        <span class={`dep-tag dep-tag-${t.tone}`} title={t.title}>
          {t.label}
        </span>
      )}
    </For>
  );
}
