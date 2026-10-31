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
import LibraryReview from "./routes/LibraryReview";
import Account from "./routes/Account";
import ConfirmRequest from "./routes/ConfirmRequest";

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
      {/* Install or upgrade a library (ADR-0076 §4): its own page, so the
          review can be sent to a second reviewer. */}
      <Route path="/settings/libraries/review" component={LibraryReview} />
      {/* Your sign-in accounts (ADR-0069) — everyone's, not admin-gated. */}
      <Route path="/account" component={Account} />
      {/* The same page, scrolled to Security (ADR-0073): where every "enroll
          a passkey" link and the fresh-login redirect land (`?enroll=1`). */}
      <Route path="/account/security" component={Account} />
      {/* A machine's StepUpConfirmationRequest (ADR-0073 §6): the
          `confirmation_url` its human opens. Three segments, so it can't
          collide with the two-segment /:org/:repo tier. */}
      <Route path="/security/confirm/:id" component={ConfirmRequest} />
      {/* What is live where (ADR-0070), org-scoped; `?env=` picks the
          Environment. One segment, like /settings. */}
      <Route path="/:org/:repo/run" component={RunPipeline} />
      <Route path="/:org/:repo/runs/:id" component={RunDetail} />
      <Route path="/:org/:repo" component={RepoView} />
    </Router>
    </Show>
  );
}
