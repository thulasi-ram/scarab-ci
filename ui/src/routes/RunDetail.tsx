// Run detail route: status + step list for one run, via the generated client.
import { createResource, For, Show } from "solid-js";
import { getRun } from "../api/client";

export default function RunDetail(props: { id: string; onBack: () => void }) {
  const [run] = createResource(() => props.id, getRun);

  return (
    <section>
      <button onClick={props.onBack}>← runs</button>
      <h1>Run {props.id}</h1>
      <Show when={run()} fallback={<p>loading…</p>}>
        {(r) => (
          <>
            <p>
              status: <strong>{r().status}</strong>
            </p>
            <h2>Steps</h2>
            <ul>
              <For each={r().steps}>
                {(step) => (
                  <li>
                    {step.id} — {step.status} ({step.attempts} attempt
                    {step.attempts === 1 ? "" : "s"})
                  </li>
                )}
              </For>
            </ul>
          </>
        )}
      </Show>
    </section>
  );
}
