// Dashboard — the engineer's landing tier. First-person, keyed on the signed-in
// actor: (1) an ACTION INBOX of things frozen until you act, (2) YOUR ACTIVITY
// across all repos with in-flight runs pinned, (3) YOUR REPOS as the navigation
// floor. When the inbox and in-flight list are both empty the page collapses to
// a deliberate "all clear" state + the repo grid — never an idle/blank surface.
//
// Representative until the forge/RepoStore + ADR-0037 gate backends land (see
// data/catalog). The `state:` pill is a PROTOTYPE-ONLY control to preview the
// active vs all-clear layouts side by side; it goes away once data is real.
import { For, Show, createSignal, createMemo } from "solid-js";
import { A, useNavigate } from "@solidjs/router";
import {
  listRepos,
  actionInbox,
  myActivity,
  ORG,
  ME,
  TRIGGER_GLYPH,
  type InboxKind,
  type InboxItem,
  type ActivityRow,
} from "../data/catalog";
import Icon from "../components/Icon";
import Sparkline from "../components/Sparkline";
import AsciiScene from "../components/AsciiScene";
import dungroller from "../../../brand/ascii/generated/dungroller-bare.json";
import emblemMark from "../../../brand/ascii/generated/emblem-mark.txt?raw";

// Per-kind presentation for an inbox row: the verb chip carries the row (the
// leading icon was redundant with it), plus the action button + its weight.
const KIND: Record<InboxKind, { verb: string; action: string; btn: string }> = {
  approve: { verb: "approve", action: "Approve", btn: "btn-primary" },
  input: { verb: "input", action: "Provide input", btn: "btn-copper" },
  rerun: { verb: "re-run", action: "Re-run", btn: "btn-ghost" },
  resume: { verb: "resume", action: "Resume", btn: "btn-ghost" },
};

type DemoState = "active" | "allclear";

export default function Repos() {
  const nav = useNavigate();
  const repos = listRepos();

  // Prototype-only: flip the whole page between populated and all-clear.
  const [state, setState] = createSignal<DemoState>("active");
  const inbox = createMemo<InboxItem[]>(() => (state() === "active" ? actionInbox() : []));
  // The feed is "observe", the inbox is "act" — a run that's waiting on you shows
  // in one place, not both. Suppress inbox runs from the activity feed by id.
  const activity = createMemo<ActivityRow[]>(() => {
    if (state() !== "active") return [];
    const inInbox = new Set(inbox().map((it) => it.id));
    return myActivity().filter((r) => !inInbox.has(r.id));
  });
  const inFlight = createMemo(() => activity().filter((r) => r.status === "running" || r.status === "pending").length);
  const calm = createMemo(() => inbox().length === 0 && inFlight() === 0);

  const openRun = (repo: string, id: string) => nav(`/${ORG}/${repo}/runs/${id}`);
  // The footer beetle rolls while work is in flight — motion literally means
  // "something is working". Quiet dashboard → it parks beside its ball.
  const rolling = createMemo(() => inFlight() > 0);

  return (
    <>
    <section class="page">
      {/* Faint brand mark in the background — the traced emblem as ASCII
          (ui/brand/ascii). Static on purpose: nothing animates behind data. */}
      <pre class="ascii-watermark" aria-hidden="true">{emblemMark}</pre>

      <div class="page-head">
        <h1>Dashboard</h1>
        <div class="dash-toggle">
          <For each={["active", "allclear"] as DemoState[]}>
            {(s) => (
              <button class={`fpill ${state() === s ? "on" : ""}`} onClick={() => setState(s)}>
                {s === "active" ? "active" : "all clear"}
              </button>
            )}
          </For>
        </div>
      </div>
      <p class="subtle page-sub">
        {ORG} · signed in as {ME} <span class="repr-tag">prototype state</span>
      </p>

      {/* ── Waiting on you (action inbox) ─────────────────────────────────── */}
      <Show
        when={inbox().length > 0}
        fallback={
          <div class="allclear">
            <Icon icon="shield-check" size={22} class="ac-ico" />
            <div>
              <div class="ac-title">All clear — nothing needs you</div>
              <div class="ac-sub">
                No approvals, inputs, or held runs waiting. New work will surface here.
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
            {(it) => {
              const k = KIND[it.kind];
              return (
                <div class="inbox-row" onClick={() => openRun(it.repo, it.id)}>
                  <span class={`inbox-verb verb-${it.kind}`}>{k.verb}</span>
                  <span class="inbox-what">
                    <span class="inbox-msg">
                      <span class="sha mono">{it.sha}</span>
                      <span class="repo">{it.repo}</span> · {it.message}
                    </span>
                    <span class="inbox-detail">
                      {it.detail}
                      <Show when={!it.real}><span class="repr-tag">representative</span></Show>
                    </span>
                  </span>
                  <span class="inbox-age">{it.age}</span>
                  <button
                    class={`btn btn-sm ${k.btn}`}
                    onClick={(e) => { e.stopPropagation(); openRun(it.repo, it.id); }}
                  >
                    {k.action}
                  </button>
                </div>
              );
            }}
          </For>
        </div>
      </Show>

      {/* ── Your activity (in-flight pinned) ──────────────────────────────── */}
      <Show when={activity().length > 0}>
        <div class="dash-label">
          your activity
          <Show when={inFlight() > 0}>
            <span class="count">· {inFlight()} in flight</span>
          </Show>
        </div>
        <div class="runlist">
          <div class="runrow head">
            <span></span><span>commit</span><span>trigger · branch</span><span>duration</span><span>when</span>
          </div>
          <For each={activity()}>
            {(r) => (
              <div class="runrow" onClick={() => openRun(r.repo, r.id)}>
                <span class={`sdot ${r.status}`} />
                <span class="rr-commit">
                  <span class="rr-sha mono">{r.sha}</span>
                  <span class="rr-msg"><span class="rr-repo">{r.repo}</span> {r.message}</span>
                </span>
                <span class="rr-trigger mono">
                  <span class="tglyph">{TRIGGER_GLYPH[r.trigger]}</span>
                  {r.trigger}
                  <span class="rr-branch"> · {r.branch}</span>
                </span>
                <span class="rr-dur mono">{r.duration}</span>
                <span class="rr-when mono">{r.age}</span>
              </div>
            )}
          </For>
        </div>
      </Show>

      {/* ── Your repos (navigation floor) ─────────────────────────────────── */}
      <div class="dash-label">
        your repos <span class="count">· {repos.length}</span>
      </div>
      <div class="repo-grid">
        <For each={repos}>
          {(r) => (
            <A href={`/${r.org}/${r.name}`} class="repo-card">
              <div class="repo-card-head">
                <span class={`sdot ${r.lastStatus}`} />
                <span class="repo-name">
                  <span class="repo-org">{r.org} /</span> {r.name}
                </span>
                <Icon icon="chevron-right" size={15} class="repo-go" />
              </div>
              <div class="repo-meta mono">
                <Icon icon="git-branch" size={12} /> {r.defaultBranch}
                <span class="dotsep">·</span>
                <span class={`facet ${r.lastStatus}`}>{r.lastStatus}</span>
              </div>
              <Sparkline runs={r.spark} />
            </A>
          )}
        </For>
      </div>
    </section>

    {/* ── Footer: the dung-roller (ui/brand/ascii) ─────────────────────────
        Scroll past everything and find the beetle walking its ball across the
        page on the copper ground line. CSS carries the travel; baked frames
        carry the legs and the rolling ball. */}
    <footer class="dash-footer" classList={{ rolling: rolling() }} aria-hidden="true">
      <div class="roller">
        <AsciiScene scene={dungroller} fontSize={4} playing={rolling()} />
      </div>
    </footer>
    </>
  );
}
