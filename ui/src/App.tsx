// Minimal shell: runs list ↔ run detail, plus a Secrets settings page. A real
// router + the rich live-DAG / SSE-logs views are the attended follow-up
// (ADR-0028); this dogfoods the generated client.
import { createSignal, Match, Switch } from "solid-js";
import RunsList from "./routes/RunsList";
import RunDetail from "./routes/RunDetail";
import Secrets from "./routes/Secrets";

type View = { name: "runs" } | { name: "run"; id: string } | { name: "secrets" };

export default function App() {
  const [view, setView] = createSignal<View>({ name: "runs" });

  return (
    <main>
      <nav>
        <button onClick={() => setView({ name: "runs" })}>Runs</button>{" "}
        <button onClick={() => setView({ name: "secrets" })}>Secrets</button>
      </nav>
      <Switch>
        <Match when={view().name === "runs"}>
          <RunsList onOpen={(id) => setView({ name: "run", id })} />
        </Match>
        <Match when={view().name === "run"}>
          <RunDetail
            id={(view() as { name: "run"; id: string }).id}
            onBack={() => setView({ name: "runs" })}
          />
        </Match>
        <Match when={view().name === "secrets"}>
          <Secrets onBack={() => setView({ name: "runs" })} />
        </Match>
      </Switch>
    </main>
  );
}
