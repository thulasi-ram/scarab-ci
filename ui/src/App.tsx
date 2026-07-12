// Minimal two-route shell (runs list ↔ run detail). A real router + the rich
// live-DAG / SSE-logs / restart-resume / time-travel views are the attended
// follow-up (ADR-0028); this is the scaffold that dogfoods the generated client.
import { createSignal, Show } from "solid-js";
import RunsList from "./routes/RunsList";
import RunDetail from "./routes/RunDetail";

export default function App() {
  const [runId, setRunId] = createSignal<string | null>(null);

  return (
    <main>
      <Show
        when={runId()}
        fallback={<RunsList onOpen={(id) => setRunId(id)} />}
      >
        {(id) => <RunDetail id={id()} onBack={() => setRunId(null)} />}
      </Show>
    </main>
  );
}
