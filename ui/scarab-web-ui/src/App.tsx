// App = the client-side router (ADR-0028). Repo-first, deep-linkable tiers:
// repos → repo → run. Each is a real URL, shareable and refreshable.
import { Router, Route } from "@solidjs/router";
import Layout from "./Layout";
import Repos from "./routes/Repos";
import RepoView from "./routes/RepoView";
import RunDetail from "./routes/RunDetail";
import RunPipeline from "./routes/RunPipeline";
import StepsProto from "./routes/StepsProto"; // PROTOTYPE — throwaway (ADR-0056 follow-up)

export default function App() {
  return (
    <Router root={Layout}>
      <Route path="/" component={Repos} />
      <Route path="/:org/:repo/run" component={RunPipeline} />
      {/* PROTOTYPE route — delete with StepsProto once a direction is picked. */}
      <Route path="/:org/:repo/runs/:id/proto" component={StepsProto} />
      <Route path="/:org/:repo/runs/:id" component={RunDetail} />
      <Route path="/:org/:repo" component={RepoView} />
    </Router>
  );
}
