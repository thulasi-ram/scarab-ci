// Dashboard — the engineer's landing tier, driven entirely by the real API:
// (1) an ACTION INBOX of suspended runs (a gate is holding them until someone
// decides), (2) recent ACTIVITY across all runs, (3) the registered PROJECTS
// (`GET /v1/repos`) as the navigation floor. When the inbox and in-flight list
// are both empty the page collapses to a deliberate "all clear" state + the
// repo grid — never an idle/blank surface.
import { For, Show, createMemo, createResource } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import { listProjects, listRuns, type Project, type RunSummary } from "../api/client";
import { relTime, absTime } from "../fmt";
import Icon from "../components/Icon";
import Doodle from "../components/Doodle";
import AsciiScene from "../components/AsciiScene";
import dungroller from "../../../brand/ascii/generated/dungroller-bare.json";
import emblemMark from "../../../brand/ascii/generated/emblem-mark.txt?raw";

// Untenanted runs (inline `POST /v1/runs` dev runs carry no org/project) still
// deep-link to the run page under the "api/unknown" placeholder tenant —
// RunDetail only uses the path segments for breadcrumbs, so the run itself
// renders fully. ONE consistent rule for inbox + activity rows.
const runHref = (r: RunSummary) => `/${r.org ?? "api"}/${r.project ?? "unknown"}/runs/${r.id}`;
const tenant = (r: RunSummary) => (r.org && r.project ? `${r.org}/${r.project}` : null);
const shortId = (id: string) => id.slice(0, 8);

export default function Repos() {
  const nav = useNavigate();
  const [projects] = createResource(listProjects);
  const [runs] = createResource(() => listRuns(50));

  // The feed is "observe", the inbox is "act" — a run that's waiting on you
  // (status "suspended": a gate holds it) shows in one place, not both.
  const inbox = () => (runs() ?? []).filter((r) => r.status === "suspended");
  const activity = () => (runs() ?? []).filter((r) => r.status !== "suspended");
  const inFlight = () =>
    activity().filter((r) => r.status === "running" || r.status === "pending").length;

  // The repo card's status facet: the tenant's most recent run (real, or none).
  const lastRun = (p: Project) =>
    (runs() ?? []).find((r) => r.org === p.org && r.project === p.project);

  const loading = () => projects.loading || runs.loading;

  // The footer beetle rolls while work is in flight (now REAL in-flight runs)
  // — motion literally means "something is working"; quiet dashboard → it
  // parks beside its ball.
  const rolling = createMemo(() => inFlight() > 0);

  return (
    <>
    <section class="page">
      {/* Faint brand mark in the background — the traced emblem as ASCII
          (ui/brand/ascii). Static on purpose: nothing animates behind data. */}
      <pre class="ascii-watermark" aria-hidden="true">{emblemMark}</pre>

      <div class="page-head">
        <h1>Dashboard</h1>
      </div>

      <Show when={runs.error || projects.error}>
        <p class="error">
          {runs.error ? "Could not load runs." : "Could not load repositories."} Is the server up?
        </p>
      </Show>

      <Show when={!loading()} fallback={<p class="empty">loading…</p>}>
        {/* ── Waiting on you (action inbox: suspended runs) ─────────────────── */}
        <Show
          when={inbox().length > 0}
          fallback={
            <div class="allclear">
              <Icon icon="shield-check" size={22} class="ac-ico" />
              <div>
                <div class="ac-title">All clear — nothing needs you</div>
                <div class="ac-sub">
                  No runs are suspended on a gate. New work will surface here.
                </div>
              </div>
            </div>
          }
        >
          <div class="dash-label">
            waiting on you <span class="count">· {inbox().length}</span>
          </div>
          <div class="inbox">
            <For each={inbox()}>
              {(r) => (
                <div class="inbox-row" onClick={() => nav(runHref(r))}>
                  <span class="inbox-verb verb-approve">approve</span>
                  <span class="inbox-what">
                    <span class="inbox-msg">
                      approve gate — run <span class="sha mono">{shortId(r.id)}</span>
                      <Show when={tenant(r)}>
                        {(t) => <span class="repo"> {t()}</span>}
                      </Show>
                    </span>
                    <span class="inbox-detail">suspended on a gate · waiting for a decision</span>
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

        {/* ── Recent activity (real runs, newest first) ─────────────────────── */}
        <div class="dash-label">
          activity
          <Show when={inFlight() > 0}>
            <span class="count">· {inFlight()} in flight</span>
          </Show>
        </div>
        <Show when={activity().length > 0} fallback={<p class="empty">No recent activity.</p>}>
          <div class="runlist">
            <div class="runrow head">
              <span></span>
              <span>run</span>
              <span>status</span>
              <span></span>
              <span>when</span>
            </div>
            <For each={activity()}>
              {(r) => (
                <div class="runrow" onClick={() => nav(runHref(r))}>
                  <span class={`sdot ${r.status}`} />
                  <span class="rr-commit">
                    <span class="rr-sha mono">{shortId(r.id)}</span>
                    <Show when={tenant(r)}>
                      {(t) => <span class="rr-msg"><span class="rr-repo">{t()}</span></span>}
                    </Show>
                  </span>
                  <span class="rr-trigger mono">{r.status}</span>
                  <span class="rr-dur mono"></span>
                  <span class="rr-when mono" title={absTime(r.created_at)}>
                    {relTime(r.created_at)}
                  </span>
                </div>
              )}
            </For>
          </div>
        </Show>

        {/* ── Registered repos (navigation floor) ───────────────────────────── */}
        <div class="dash-label">
          repos <span class="count">· {projects()?.length ?? 0}</span>
        </div>
        <Show
          when={(projects()?.length ?? 0) > 0}
          fallback={<p class="empty">No repos registered yet.</p>}
        >
          <div class="repo-grid">
            <For each={projects()}>
              {(p) => {
                const last = () => lastRun(p);
                return (
                  <A href={`/${p.org}/${p.project}`} class="repo-card">
                    <div class="repo-card-head">
                      <span class={`sdot ${last()?.status ?? "pending"}`} />
                      <span class="repo-name">
                        <span class="repo-org">{p.org} /</span> {p.project}
                      </span>
                      <Icon icon="chevron-right" size={15} class="repo-go" />
                    </div>
                    <div class="repo-meta mono">
                      <Icon icon="git-branch" size={12} /> {p.owner}/{p.name}
                      <Show when={last()}>
                        {(l) => (
                          <>
                            <span class="dotsep">·</span>
                            <span class={`facet ${l().status}`}>{l().status}</span>
                            <span class="dotsep">·</span>
                            <span>{relTime(l().created_at)}</span>
                          </>
                        )}
                      </Show>
                    </div>
                  </A>
                );
              }}
            </For>
          </div>
        </Show>
      </Show>
    </section>

    {/* ── Footer: the dung-roller (ui/brand/ascii) ─────────────────────────
        Scroll past everything and find the beetle walking its ball across the
        page on the copper ground line. CSS carries the travel; baked frames
        carry the legs and the rolling ball. */}
    <footer class="dash-footer" classList={{ rolling: rolling() }} aria-hidden="true">
      <div class="roller">
        <AsciiScene scene={dungroller} fontSize={5} playing={rolling()} />
      </div>
    </footer>
    </>
  );
}
