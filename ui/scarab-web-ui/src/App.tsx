// App = the client-side router (ADR-0028). Repo-first, deep-linkable tiers:
// repos → repo → run. Each is a real URL, shareable and refreshable.
import { Router, Route } from "@solidjs/router";
import Layout from "./Layout";
import Repos from "./routes/Repos";
import RepoView from "./routes/RepoView";
import RunDetail from "./routes/RunDetail";
import RunPipeline from "./routes/RunPipeline";
import Settings from "./routes/Settings";

export default function App() {
  return (
    <Router root={Layout}>
      <Route path="/" component={Repos} />
      {/* Global, org-scoped settings (ADR-0060). One segment, so it can't
          collide with the two-segment /:org/:repo tier. */}
      <Route path="/settings" component={Settings} />
      <Route path="/:org/:repo/run" component={RunPipeline} />
      <Route path="/:org/:repo/runs/:id" component={RunDetail} />
      <Route path="/:org/:repo" component={RepoView} />
    </Router>
  );
}
