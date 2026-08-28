// App = the client-side router (ADR-0028). Repo-first, deep-linkable tiers:
// repos → repo → run. Each is a real URL, shareable and refreshable.
import { Show } from "solid-js";
import { Router, Route } from "@solidjs/router";
import Layout from "./Layout";
import Login from "./Login";
import { unauthenticated } from "./api/client";
import Repos from "./routes/Repos";
import RepoView from "./routes/RepoView";
import RunDetail from "./routes/RunDetail";
import RunPipeline from "./routes/RunPipeline";
import Settings from "./routes/Settings";

export default function App() {
  // The sign-in gate sits ABOVE the Router, not inside its root, and that
  // placement is the whole point (ADR-0049).
  //
  // Gating inside `root={Layout}` looks equivalent and is not: flipping it
  // unmounts the router's OUTLET while the route context around it is still
  // live, and solid-router's per-route memo then re-runs with no match and
  // throws `Cannot read properties of undefined (reading 'path')` — as an
  // unhandled promise rejection, because the 401 that flips the signal arrives
  // in a fetch callback. Gating here unmounts the whole Router at once, so
  // there is no half-torn-down routing tree to re-evaluate.
  return (
    <Show when={!unauthenticated()} fallback={<Login />}>
    <Router root={Layout}>
      <Route path="/" component={Repos} />
      {/* Global, org-scoped settings (ADR-0060). One segment, so it can't
          collide with the two-segment /:org/:repo tier. */}
      <Route path="/settings" component={Settings} />
      <Route path="/:org/:repo/run" component={RunPipeline} />
      <Route path="/:org/:repo/runs/:id" component={RunDetail} />
      <Route path="/:org/:repo" component={RepoView} />
    </Router>
    </Show>
  );
}
