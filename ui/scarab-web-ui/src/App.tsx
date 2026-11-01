// App = the client-side router. Repo-first, deep-linkable tiers:
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
import { AccountSettings, LegacySettings, OrgSettings, ServerSettings } from "./routes/Settings";
import LibraryReview from "./routes/LibraryReview";
import { EnvironmentEdit, EnvironmentView } from "./routes/EnvironmentPage";
import ConfirmRequest from "./routes/ConfirmRequest";

export default function App() {
  // The sign-in gate sits ABOVE the Router, not inside its root, and that
  // placement is the whole point.
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
      {/* Settings (git-bug 734ffa1): Your account under /settings/<section>,
          an org's settings under /orgs/<org>/settings/<section>. Static first
          segments, so they outrank the /:org/:repo tier. */}
      <Route path="/settings" component={LegacySettings} />
      <Route path="/settings/:section" component={AccountSettings} />
      <Route path="/settings/server/:section" component={ServerSettings} />
      <Route path="/orgs/:org/settings/:section?" component={OrgSettings} />
      {/* Install or upgrade a library: its own URL inside the settings
          frame, so the review can be sent to a second reviewer. */}
      <Route path="/orgs/:org/settings/libraries/review" component={LibraryReview} />
      {/* One Environment (git-bug c11abc5): read-only, and its edit screen,
          both inside the settings frame. Five and six segments under the static /orgs, so they collide with
          neither the settings sections nor /:org/:repo/runs/:id; the name is
          one encoded segment. */}
      <Route path="/orgs/:org/settings/environments/:env" component={EnvironmentView} />
      <Route path="/orgs/:org/settings/environments/:env/edit" component={EnvironmentEdit} />
      {/* Old addresses, kept for bookmarks and links sent before the move. */}
      <Route path="/settings/libraries/review" component={LegacySettings} />
      <Route path="/account" component={LegacySettings} />
      <Route path="/account/security" component={LegacySettings} />
      {/* A machine's StepUpConfirmationRequest: the
          `confirmation_url` its human opens. Three segments, so it can't
          collide with the two-segment /:org/:repo tier. */}
      <Route path="/security/confirm/:id" component={ConfirmRequest} />
      {/* What is live where, org-scoped; `?env=` picks the
          Environment. One segment, like /settings. */}
      <Route path="/:org/:repo/run" component={RunPipeline} />
      <Route path="/:org/:repo/runs/:id" component={RunDetail} />
      <Route path="/:org/:repo" component={RepoView} />
    </Router>
    </Show>
  );
}
