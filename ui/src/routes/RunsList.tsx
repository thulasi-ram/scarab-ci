// Runs list route. A dedicated list endpoint (GET /v1/runs) is an attended
// follow-up — for now this route opens a run by id, which is enough to
// dogfood the run-detail path. Kept a shell per the scaffold-only scope.
import { createSignal } from "solid-js";

export default function RunsList(props: { onOpen: (id: string) => void }) {
  const [id, setId] = createSignal("");

  return (
    <section>
      <h1>Runs</h1>
      <p>Open a run by id (a list endpoint lands with the full UI):</p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (id().trim()) props.onOpen(id().trim());
        }}
      >
        <input
          value={id()}
          onInput={(e) => setId(e.currentTarget.value)}
          placeholder="run id"
        />
        <button type="submit">Open</button>
      </form>
    </section>
  );
}
