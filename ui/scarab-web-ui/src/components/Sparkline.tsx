// Tiny run-history sparkline: one bar per recent run, colored by outcome. Bar
// heights vary deterministically by index so it reads as history, not noise.
import { For } from "solid-js";
import type { RunFacet } from "../data/catalog";

const HEIGHTS = [58, 80, 46, 70, 90, 54, 76, 64];

export default function Sparkline(props: { runs: RunFacet[] }) {
  return (
    <div class="spark" aria-hidden="true">
      <For each={props.runs}>
        {(f, i) => (
          <i class={`spark-bar ${f}`} style={{ height: `${HEIGHTS[i() % HEIGHTS.length]}%` }} />
        )}
      </For>
    </div>
  );
}
