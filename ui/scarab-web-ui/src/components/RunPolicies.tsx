// The Run page's Policies strip: every recorded decision of the Run that a
// Policy's `match` selected, oldest first. Each names the checkpoint (and the
// Step and attempt for a launch), each Policy's outcome with its message, and
// opens the input the rules saw. A Run no Policy applied to shows nothing.
//
// While the Run is live, the list is read again every 10 seconds: each Step
// launch records a decision.
import { createEffect, createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { listRunPolicyDecisions } from "../api/client";
import {
  RUN_OUTCOME_LABEL,
  runDecisionViews,
  runPolicySummary,
  type DecisionRecordLike,
  type RunDecisionView,
} from "../policies";
import { absTime, relTime } from "../fmt";
import Pill from "./Pill";

export default function RunPolicies(props: { runId: string; live: boolean }) {
  const [tick, setTick] = createSignal(0);
  createEffect(() => {
    if (!props.live) return;
    const t = setInterval(() => setTick((n) => n + 1), 10_000);
    onCleanup(() => clearInterval(t));
  });
  // A Run whose decisions cannot be read shows no strip: the rest of the page
  // does not depend on them.
  const [decisions] = createResource(
    () => [props.runId, tick()] as const,
    ([id]) => listRunPolicyDecisions(id).catch(() => []),
  );
  const views = () =>
    runDecisionViews(
      (decisions.latest ?? []).map((d) => ({ ...d, decision: d.decision as unknown as DecisionRecordLike["decision"] })),
    );

  return (
    <Show when={views().length > 0}>
      <details class="panel run-policies" open={views().some((v) => v.verdict === "denied")}>
        <summary class="panel-h">
          <span>Policies</span>
          <span class="subtle">{runPolicySummary(views())}</span>
        </summary>
        <ul class="tok-list run-pol-list" aria-label="Policy decisions">
          <For each={views()}>{(v) => <DecisionRow view={v} />}</For>
        </ul>
      </details>
    </Show>
  );
}

function DecisionRow(props: { view: RunDecisionView }) {
  const v = () => props.view;
  return (
    <li class="tok">
      <div class="tok-head">
        <Pill tone={v().verdict === "denied" ? "danger" : "copper"}>{v().verdict}</Pill>
        <span class="mono">{v().checkpoint}</span>
        <Show when={v().step}>
          {(step) => (
            <span class="mono subtle">
              {step()}
              <Show when={v().attempt}>{(n) => <> · try {n()}</>}</Show>
            </span>
          )}
        </Show>
        <span class="subtle" title={absTime(v().at)}>
          {relTime(v().at)}
        </span>
      </div>
      <ul class="run-pol-results">
        <For each={v().results}>
          {(r) => (
            <li>
              <span class={`run-pol-outcome run-pol-${r.outcome}`}>{RUN_OUTCOME_LABEL[r.outcome]}</span>
              <span class="mono">{r.name}</span>
              <Show when={r.level === "project"}>
                <span class="subtle">project setting</span>
              </Show>
              <Show when={r.messages.length > 0}>
                <span class="run-pol-msg">{r.messages.join(" · ")}</span>
              </Show>
            </li>
          )}
        </For>
      </ul>
      <Show
        when={v().input !== null}
        fallback={
          <p class="subtle">
            <small>No input was recorded: the Run was over an input size cap.</small>
          </p>
        }
      >
        <details class="run-pol-input">
          <summary>Input the rules saw</summary>
          <pre class="mono">{JSON.stringify(v().input, null, 2)}</pre>
        </details>
      </Show>
    </li>
  );
}
