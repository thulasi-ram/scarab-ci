// A repo's recent runs as a rounded pass/fail bar strip (status-page style):
// oldest → newest, green for succeeded, red for failed/cancelled, amber for a
// gate-suspended run, muted for in-flight. Fed by GET /v1/repos/{org}/{repo}/runs.
import { For } from "solid-js";
import type { RunSummary } from "../api/client";

function tone(status: string): string {
  if (status === "succeeded") return "ok";
  if (status === "failed" || status === "cancelled") return "fail";
  if (status === "suspended") return "warn";
  return "idle"; // running / pending
}

export default function RunBars(props: { runs: RunSummary[]; max?: number }) {
  // Newest first from the API; render oldest → newest so "now" is on the right.
  const bars = () => props.runs.slice(0, props.max ?? 14).reverse();
  return (
    <div class="runbars" aria-hidden="true">
      <For each={bars()}>
        {(r) => <span class={`runbar ${tone(r.status)}`} title={r.status} />}
      </For>
    </div>
  );
}
