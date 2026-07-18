// A repo's recent runs as a duration bar strip (status-page style): oldest →
// newest, one bar per run. Colour = outcome (green pass / red fail / amber gate
// / muted in-flight); HEIGHT = how long the run took (`duration_ms`), sqrt-scaled
// against the strip's peak so one long run doesn't flatten the rest, with a floor
// so short runs stay visible. Fed by GET /v1/repos/{org}/{repo}/runs.
import { For } from "solid-js";
import type { RunSummary } from "../api/client";
import { duration } from "../fmt";

function tone(status: string): string {
  if (status === "succeeded") return "ok";
  if (status === "failed" || status === "cancelled") return "fail";
  if (status === "suspended") return "warn";
  return "idle"; // running / pending
}

const FLOOR = 18; // %, so the shortest runs still read

export default function RunBars(props: { runs: RunSummary[]; max?: number }) {
  // Newest first from the API; render oldest → newest so "now" is on the right.
  const bars = () => props.runs.slice(0, props.max ?? 16).reverse();
  const peak = () => Math.max(1, ...bars().map((r) => r.duration_ms ?? 0));
  const height = (ms: number) =>
    Math.round(FLOOR + (100 - FLOOR) * Math.sqrt(Math.max(0, ms) / peak()));

  return (
    <div class="runbars" aria-hidden="true">
      <For each={bars()}>
        {(r) => (
          <span
            class={`runbar ${tone(r.status)}`}
            style={{ height: `${height(r.duration_ms ?? 0)}%` }}
            title={`${r.status} · ${duration(0, r.duration_ms ?? 0)}`}
          />
        )}
      </For>
    </div>
  );
}
