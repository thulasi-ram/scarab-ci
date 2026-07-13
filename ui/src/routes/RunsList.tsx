// Runs list route — lists recent runs from GET /v1/runs, opens one by click, and
// can create a sample run (handy for driving the local `--executor local` stack
// end-to-end from the browser). The rich live-DAG / SSE view is the attended
// follow-up (ADR-0028); this dogfoods the generated client.
import { createResource, createSignal, For, Show } from "solid-js";
import { listRuns, createRun } from "../api/client";

export default function RunsList(props: { onOpen: (id: string) => void }) {
  const [runs, { refetch }] = createResource(() => listRuns(50));
  const [id, setId] = createSignal("");
  const [busy, setBusy] = createSignal(false);

  async function runSample() {
    setBusy(true);
    try {
      await createRun({
        pipeline: {
          ir_version: 1,
          steps: [
            { id: "build", image: "x", command: ["sh", "-c", "echo building; sleep 1"] },
            { id: "test", image: "x", command: ["sh", "-c", "echo testing; sleep 1"], needs: ["build"] },
          ],
        },
      });
      await refetch();
    } finally {
      setBusy(false);
    }
  }

  return (
    <section>
      <h1>Runs</h1>
      <p>
        <button onClick={runSample} disabled={busy()}>
          {busy() ? "starting…" : "Run a sample pipeline"}
        </button>{" "}
        <button onClick={() => refetch()}>Refresh</button>
      </p>

      <Show when={!runs.loading} fallback={<p>loading…</p>}>
        <Show when={(runs()?.length ?? 0) > 0} fallback={<p>No runs yet.</p>}>
          <table>
            <thead>
              <tr><th>id</th><th>status</th><th>created</th></tr>
            </thead>
            <tbody>
              <For each={runs()}>
                {(r) => (
                  <tr style={{ cursor: "pointer" }} onClick={() => props.onOpen(r.id)}>
                    <td><code>{r.id.slice(0, 8)}</code></td>
                    <td>{r.status}</td>
                    <td>{new Date(r.created_at).toLocaleTimeString()}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </table>
        </Show>
      </Show>

      <p>Or open a run by id:</p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (id().trim()) props.onOpen(id().trim());
        }}
      >
        <input value={id()} onInput={(e) => setId(e.currentTarget.value)} placeholder="run id" />
        <button type="submit">Open</button>
      </form>
    </section>
  );
}
