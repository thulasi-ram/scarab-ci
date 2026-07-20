// PROTOTYPE — throwaway. Explores the "unify STEPS + attempts + evidence" idea
// (ADR-0056 follow-up): where does a step's attempt CARD-STACK live, and how do
// Graph/Timeline sub-views feel? Three structurally different variants on one
// route, real data from the run in the URL, switchable via ?variant= and a
// floating bar. Delete once a direction is picked; fold the winner into
// RunDetail. NOT production — evidence (logs) is stubbed; layout is the question.
import { createResource, createSignal, createMemo, For, Show } from "solid-js";
import { useParams, useSearchParams } from "@solidjs/router";
import { getRun, fetchEvents, type StepStatus, type Attempt } from "../api/client";
import { attemptCauses, type AttemptCause } from "../takes";

type View = "graph" | "timeline";
type Try = { a: Attempt; i: number; cause?: AttemptCause; superseded: boolean; shadowed: boolean };

const causeSuffix = (c?: AttemptCause) =>
  c === "rerun" ? " · you reran" : c === "cascade" ? " · ⟵ rerun" : c === "retry" ? " · auto-retry" : "";
const tryTitle = (t: Try) => `try ${t.i + 1}${causeSuffix(t.cause)}`;
const tryOutcome = (t: Try) =>
  t.superseded ? "⊘ superseded" : t.a.failed ? `✗ failed${t.a.failure ? ` · ${t.a.failure}` : ""}` : "✓ succeeded";
const tryTone = (t: Try) => (t.superseded ? "var(--copper)" : t.a.failed ? "var(--danger)" : "var(--emerald)");

function statusColor(s: string): string {
  if (s === "running") return "var(--emerald-bright)";
  if (s === "succeeded") return "var(--emerald)";
  if (s === "failed" || s === "dead_lettered") return "var(--danger)";
  if (s === "cancelled") return "var(--copper)";
  return "var(--muted-sage)";
}

export default function StepsProto() {
  const params = useParams();
  const [search, setSearch] = useSearchParams();
  const [run] = createResource(() => params.id, getRun);
  const [events] = createResource(() => params.id, fetchEvents);

  const variant = () => (search.variant as string) ?? "A";
  const [view, setView] = createSignal<View>("graph");
  const [selStep, setSelStep] = createSignal<string | null>(null);
  const [selTry, setSelTry] = createSignal<string | null>(null);

  const steps = (): StepStatus[] => run()?.steps ?? [];
  const step = (id: string | null) => steps().find((s) => s.id === id) ?? null;
  const current = () => step(selStep()) ?? steps()[0] ?? null;

  const triesOf = (s: StepStatus | null): Try[] => {
    if (!s) return [];
    const c = attemptCauses(events() ?? [], s.id);
    return (s.attempt_list ?? []).map((a, i) => ({
      a,
      i,
      cause: c.causes[a.id],
      superseded: c.superseded.has(a.id),
      shadowed: c.shadowed.has(a.id),
    }));
  };
  const currentTries = createMemo(() => triesOf(current()));
  const activeTry = () => {
    const ts = currentTries();
    return ts.find((t) => t.a.id === selTry()) ?? ts[ts.length - 1] ?? null;
  };
  const pick = (stepId: string, tryId?: string) => {
    setSelStep(stepId);
    setSelTry(tryId ?? null);
  };

  // Timeline timing (approx, off the event log).
  const stepEnd = (id: string): number | null => {
    let end: number | null = null;
    for (const e of events() ?? []) {
      const k = e.kind as unknown;
      if (k && typeof k === "object" && "StepTransitioned" in (k as Record<string, unknown>)) {
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        const v = (k as any).StepTransitioned;
        if (v.step === id && ["Succeeded", "Failed", "Cancelled", "Skipped"].includes(v.to)) end = e.at;
      }
    }
    return end;
  };
  const timing = createMemo(() => {
    const rows = steps().map((s) => {
      const starts = (s.attempt_list ?? []).map((a) => a.started_at).sort((a, b) => a - b);
      const start = starts[0] ?? null;
      const end = stepEnd(s.id) ?? (start != null ? start + 1000 : null);
      return { s, start, end };
    });
    const withStart = rows.filter((r) => r.start != null);
    const lo = withStart.length ? Math.min(...withStart.map((r) => r.start!)) : 0;
    const hi = rows.filter((r) => r.end != null).length
      ? Math.max(...rows.filter((r) => r.end != null).map((r) => r.end!))
      : lo + 1;
    return { rows, lo, span: Math.max(1, hi - lo) };
  });

  const Toggle = () => (
    <div class="pp-toggle">
      <button classList={{ on: view() === "graph" }} onClick={() => setView("graph")}>Graph</button>
      <button classList={{ on: view() === "timeline" }} onClick={() => setView("timeline")}>Timeline</button>
    </div>
  );
  const Dot = (p: { status: string }) => <span class="pp-dot" style={{ background: statusColor(p.status) }} />;

  function TimelineView() {
    return (
      <div class="pp-timeline">
        <For each={timing().rows}>
          {(r) => {
            const ts = () => triesOf(r.s);
            const left = () => (r.start != null ? ((r.start - timing().lo) / timing().span) * 100 : 0);
            const width = () =>
              r.start != null && r.end != null ? Math.max(2, ((r.end - r.start) / timing().span) * 100) : 2;
            return (
              <div class="pp-tlrow" classList={{ sel: current()?.id === r.s.id }} onClick={() => pick(r.s.id)}>
                <div class="pp-tlname"><Dot status={r.s.status} /> {r.s.id}</div>
                <div class="pp-tltrack">
                  <div class="pp-tlbar" style={{ left: `${left()}%`, width: `${width()}%` }}>
                    <For each={ts()}>
                      {(t, i) => (
                        <div
                          class="pp-tlseg"
                          title={`${tryTitle(t)} — ${tryOutcome(t)}`}
                          style={{
                            flex: 1,
                            background: tryTone(t),
                            opacity: t.shadowed ? 0.5 : 1,
                            "border-right": i() < ts().length - 1 ? "1px solid var(--pine-deep)" : "none",
                          }}
                        />
                      )}
                    </For>
                  </div>
                </div>
              </div>
            );
          }}
        </For>
      </div>
    );
  }

  function Evidence(p: { compact?: boolean }) {
    return (
      <div class="pp-ev" classList={{ compact: p.compact }}>
        <div class="pp-tabs">
          <For each={["Logs", "Results", "Outputs", "Workspace"]}>
            {(t, i) => <button classList={{ on: i() === 0 }}>{t}</button>}
          </For>
        </div>
        <Show when={activeTry()} fallback={<div class="pp-evbody pp-muted">select a step</div>}>
          {(t) => (
            <>
              <div class="pp-evhead">
                {current()?.id} · {tryTitle(t())} ·{" "}
                <span style={{ color: tryTone(t()) }}>{tryOutcome(t())}</span>
              </div>
              <div class="pp-evbody">
                <pre class="pp-log">{`+ running ${current()?.id} (${t().a.id})
info: stubbed prototype log body — layout only
info: real logs stream over SSE in the app
ok. done`}</pre>
              </div>
            </>
          )}
        </Show>
      </div>
    );
  }

  // A — deck of cards IN the graph; selecting fans the tries; evidence at right.
  function VariantA() {
    return (
      <div class="pp-grid">
        <div class="pp-col">
          <div class="pp-collabel">Steps</div>
          <Show when={view() === "graph"} fallback={<TimelineView />}>
            <div class="pp-nodes">
              <For each={steps()}>
                {(s) => {
                  const ts = () => triesOf(s);
                  const depth = () => Math.min(ts().length, 4);
                  const sel = () => current()?.id === s.id;
                  return (
                    <div class="pp-nodewrap">
                      <div class="pp-deck" classList={{ sel: sel() }} onClick={() => pick(s.id)}>
                        <For each={Array.from({ length: Math.max(0, depth() - 1) })}>
                          {(_, i) => (
                            <div class="pp-decklayer" style={{ transform: `translate(${(i() + 1) * 3}px, ${(i() + 1) * 3}px)` }} />
                          )}
                        </For>
                        <div class="pp-node">
                          <Dot status={s.status} />
                          <span class="pp-nodeid">{s.id}</span>
                          <Show when={ts().length > 1}><span class="pp-count">×{ts().length}</span></Show>
                        </div>
                      </div>
                      <Show when={sel() && ts().length > 0}>
                        <div class="pp-fan">
                          <For each={ts()}>
                            {(t) => (
                              <button
                                class="pp-trycard"
                                classList={{ on: activeTry()?.a.id === t.a.id, shadow: t.shadowed }}
                                style={{ "border-left": `3px solid ${tryTone(t)}` }}
                                onClick={() => pick(s.id, t.a.id)}
                              >
                                <div class="pp-tct">{tryTitle(t)}</div>
                                <div class="pp-tcs">{tryOutcome(t)}{t.shadowed ? " · shadowed" : ""}</div>
                              </button>
                            )}
                          </For>
                        </div>
                      </Show>
                    </div>
                  );
                }}
              </For>
            </div>
          </Show>
        </div>
        <Evidence />
      </div>
    );
  }

  // B — the selected node GROWS to contain its try-stack AND the evidence.
  function VariantB() {
    return (
      <div class="pp-col wide">
        <div class="pp-collabel">Steps</div>
        <Show when={view() === "graph"} fallback={<TimelineView />}>
          <div class="pp-nodes">
            <For each={steps()}>
              {(s) => {
                const ts = () => triesOf(s);
                const sel = () => current()?.id === s.id;
                return (
                  <div class="pp-expand" classList={{ sel: sel() }}>
                    <div class="pp-node big" onClick={() => pick(s.id)}>
                      <Dot status={s.status} />
                      <span class="pp-nodeid">{s.id}</span>
                      <Show when={ts().length > 1}><span class="pp-count">×{ts().length}</span></Show>
                      <span style={{ flex: 1 }} />
                      <span class="pp-muted">{sel() ? "▾" : "▸"}</span>
                    </div>
                    <Show when={sel()}>
                      <div class="pp-inner">
                        <div class="pp-trystack">
                          <For each={ts()}>
                            {(t) => (
                              <button
                                class="pp-trychip"
                                classList={{ on: activeTry()?.a.id === t.a.id }}
                                style={{ "border-color": activeTry()?.a.id === t.a.id ? tryTone(t) : "var(--border)" }}
                                onClick={() => pick(s.id, t.a.id)}
                              >
                                {tryTitle(t)} · <span style={{ color: tryTone(t) }}>{tryOutcome(t)}</span>
                              </button>
                            )}
                          </For>
                        </div>
                        <Evidence compact />
                      </div>
                    </Show>
                  </div>
                );
              }}
            </For>
          </div>
        </Show>
      </div>
    );
  }

  // C — clean graph; the card-stack lives in the evidence header (right).
  function VariantC() {
    return (
      <div class="pp-grid">
        <div class="pp-col">
          <div class="pp-collabel">Steps</div>
          <Show when={view() === "graph"} fallback={<TimelineView />}>
            <div class="pp-nodes">
              <For each={steps()}>
                {(s) => (
                  <div class="pp-node clean" classList={{ sel: current()?.id === s.id }} onClick={() => pick(s.id)}>
                    <Dot status={s.status} />
                    <span class="pp-nodeid">{s.id}</span>
                    <Show when={triesOf(s).length > 1}><span class="pp-count">×{triesOf(s).length}</span></Show>
                  </div>
                )}
              </For>
            </div>
          </Show>
        </div>
        <div class="pp-ev">
          <div class="pp-evtryrow">
            <For each={currentTries()}>
              {(t) => (
                <button
                  class="pp-trycard row"
                  classList={{ on: activeTry()?.a.id === t.a.id, shadow: t.shadowed }}
                  style={{ "border-top": `3px solid ${tryTone(t)}` }}
                  onClick={() => current() && pick(current()!.id, t.a.id)}
                >
                  <div class="pp-tct">{tryTitle(t)}</div>
                  <div class="pp-tcs">{tryOutcome(t)}</div>
                </button>
              )}
            </For>
          </div>
          <div class="pp-tabs">
            <For each={["Logs", "Results", "Outputs", "Workspace"]}>
              {(t, i) => <button classList={{ on: i() === 0 }}>{t}</button>}
            </For>
          </div>
          <Show when={activeTry()}>
            {(t) => (
              <div class="pp-evbody">
                <pre class="pp-log">{`+ running ${current()?.id} (${t().a.id})\ninfo: stubbed prototype log body\nok. done`}</pre>
              </div>
            )}
          </Show>
        </div>
      </div>
    );
  }

  const names: Record<string, string> = {
    A: "Deck in the DAG",
    B: "Expand in place",
    C: "Card-stack in evidence header",
  };
  const cycle = (d: number) => {
    const vs = ["A", "B", "C"];
    const i = vs.indexOf(variant());
    setSearch({ variant: vs[(i + d + vs.length) % vs.length] });
  };

  return (
    <section class="page pp">
      <style>{CSS}</style>
      <div class="pp-head">
        <h1>run {(params.id ?? "").slice(0, 8)} <span class="pp-muted">· STEPS prototype · variant {variant()}</span></h1>
        <Toggle />
      </div>

      <Show when={run()} fallback={<p class="pp-muted">loading…</p>}>
        <Show when={variant() === "A"}><VariantA /></Show>
        <Show when={variant() === "B"}><VariantB /></Show>
        <Show when={variant() === "C"}><VariantC /></Show>
      </Show>

      <Show when={(import.meta as { env?: { DEV?: boolean } }).env?.DEV ?? true}>
        <div class="pp-switch">
          <button onClick={() => cycle(-1)}>←</button>
          <span>{variant()} — {names[variant()]}</span>
          <button onClick={() => cycle(1)}>→</button>
        </div>
      </Show>
    </section>
  );
}

const CSS = `
.pp { max-width: 1200px; }
.pp-head { display: flex; align-items: center; gap: 16px; margin-bottom: 16px; }
.pp-head h1 { font-size: 20px; font-family: var(--font-mono); }
.pp-muted { color: var(--muted-sage); font-weight: 400; font-size: 13px; }
.pp-toggle { display: inline-flex; border: 1px solid var(--border); border-radius: var(--radius-sm); overflow: hidden; }
.pp-toggle button { padding: 5px 12px; font-size: 11px; font-family: var(--font-mono); text-transform: uppercase; letter-spacing: 1px; background: transparent; color: var(--muted-sage); border: none; cursor: pointer; }
.pp-toggle button.on { background: var(--emerald-surface); color: var(--soft-white); }
.pp-grid { display: grid; grid-template-columns: minmax(220px, 0.8fr) 1.2fr; gap: 16px; align-items: start; }
.pp-col { border: 1px solid var(--border); border-radius: var(--radius); padding: 12px; }
.pp-col.wide { grid-column: 1 / -1; }
.pp-collabel { font-family: var(--font-mono); font-size: 10px; text-transform: uppercase; letter-spacing: 1.4px; color: var(--muted-sage); margin-bottom: 12px; }
.pp-nodes { display: flex; flex-direction: column; gap: 22px; }
.pp-dot { width: 9px; height: 9px; border-radius: 50%; display: inline-block; }
.pp-node { display: inline-flex; align-items: center; gap: 8px; padding: 10px 14px; border: 1px solid var(--border); border-radius: 10px; background: var(--terminal-elev); color: var(--terminal-ink); font-family: var(--font-ui); position: relative; z-index: 2; cursor: pointer; }
.pp-nodeid { font-weight: 600; }
.pp-count { font-family: var(--font-mono); font-size: 10px; color: var(--muted-sage); border: 1px solid var(--border); border-radius: 20px; padding: 0 6px; }
.pp-nodewrap { display: flex; flex-direction: column; align-items: flex-start; gap: 8px; }
.pp-deck { position: relative; display: inline-block; cursor: pointer; }
.pp-deck.sel .pp-node { border-color: var(--emerald); }
.pp-decklayer { position: absolute; inset: 0; border: 1px solid var(--border); border-radius: 10px; background: var(--terminal); z-index: 1; }
.pp-fan { display: flex; gap: 8px; flex-wrap: wrap; padding-left: 10px; }
.pp-trycard { text-align: left; background: var(--pine-deep); border: 1px solid var(--border); border-radius: 8px; padding: 7px 10px; cursor: pointer; min-width: 130px; }
.pp-trycard.on { background: var(--emerald-surface); }
.pp-trycard.shadow { opacity: 0.6; }
.pp-tct { font-family: var(--font-mono); font-size: 11.5px; color: var(--soft-white); }
.pp-tcs { font-family: var(--font-mono); font-size: 10px; color: var(--muted-sage); margin-top: 2px; }
.pp-expand { border: 1px solid var(--border); border-radius: 10px; overflow: hidden; }
.pp-expand.sel { border-color: var(--emerald); }
.pp-node.big { width: 100%; border: none; border-radius: 0; }
.pp-inner { padding: 12px; border-top: 1px solid var(--border); }
.pp-trystack { display: flex; gap: 8px; flex-wrap: wrap; margin-bottom: 10px; }
.pp-trychip { font-family: var(--font-mono); font-size: 11px; background: var(--pine-deep); border: 1px solid var(--border); border-radius: 20px; padding: 4px 11px; color: var(--sage); cursor: pointer; }
.pp-trychip.on { color: var(--soft-white); background: var(--emerald-surface); }
.pp-node.clean { width: 100%; cursor: pointer; }
.pp-node.clean.sel { border-color: var(--emerald); }
.pp-evtryrow { display: flex; gap: 8px; padding: 12px 12px 0; flex-wrap: wrap; }
.pp-trycard.row { min-width: 120px; }
.pp-ev { border: 1px solid var(--border); border-radius: var(--radius); overflow: hidden; }
.pp-tabs { display: flex; gap: 2px; border-bottom: 1px solid var(--border); }
.pp-tabs button { font-family: var(--font-mono); font-size: 11px; text-transform: uppercase; letter-spacing: 0.6px; color: var(--muted-sage); background: transparent; border: none; border-bottom: 2px solid transparent; padding: 10px 14px; cursor: pointer; }
.pp-tabs button.on { color: var(--soft-white); border-bottom-color: var(--emerald); }
.pp-evhead { font-family: var(--font-mono); font-size: 12px; color: var(--soft-white); padding: 12px 14px 0; }
.pp-evbody { padding: 12px 14px; }
.pp-log { font-family: var(--font-mono); font-size: 12px; color: var(--terminal-ink); background: var(--terminal); border-radius: 8px; padding: 12px; white-space: pre-wrap; }
.pp-timeline { display: flex; flex-direction: column; gap: 8px; }
.pp-tlrow { display: grid; grid-template-columns: 130px 1fr; gap: 12px; align-items: center; cursor: pointer; padding: 4px; border-radius: 6px; }
.pp-tlrow.sel { background: var(--emerald-surface); }
.pp-tlname { font-family: var(--font-mono); font-size: 12px; color: var(--soft-white); display: flex; align-items: center; gap: 7px; }
.pp-tltrack { position: relative; height: 20px; background: var(--pine-deep); border-radius: 4px; }
.pp-tlbar { position: absolute; top: 0; bottom: 0; display: flex; border-radius: 4px; overflow: hidden; min-width: 6px; }
.pp-tlseg { height: 100%; }
.pp-switch { position: fixed; bottom: 20px; left: 50%; transform: translateX(-50%); z-index: 9999; display: inline-flex; align-items: center; gap: 12px; background: #111; color: #fff; border: 1px solid #444; border-radius: 999px; padding: 8px 16px; box-shadow: 0 6px 24px rgba(0,0,0,0.4); font-family: var(--font-mono); font-size: 12px; }
.pp-switch button { background: #333; color: #fff; border: none; border-radius: 6px; width: 28px; height: 28px; cursor: pointer; font-size: 14px; }
`;
