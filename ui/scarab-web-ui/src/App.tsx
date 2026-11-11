// App = the client-side router. Repo-first, deep-linkable tiers:
// repos → org → repo → run. Each is a real URL, shareable and refreshable.
import { Show } from "solid-js";
import { Router, Route } from "@solidjs/router";
import Layout from "./Layout";
import Login from "./Login";
import { unauthenticated } from "./api/client";
import Repos from "./routes/Repos";
import RepoView from "./routes/RepoView";
import RunDetail from "./routes/RunDetail";
import RunPipeline from "./routes/RunPipeline";
import { AccountSettings, LegacySettings, NewOrgSettings, OrgSettings } from "./routes/Settings";
import LibraryReview from "./routes/LibraryReview";
import LibraryManage from "./routes/LibraryManage";
import PluginManage from "./routes/PluginManage";
import ConnectionManage from "./routes/ConnectionManage";
import MemberManage from "./routes/MemberManage";
import { EnvironmentManage, EnvironmentRedirect } from "./routes/EnvironmentPage";
import ConfirmRequest from "./routes/ConfirmRequest";
import OrgPage from "./routes/OrgPage";
import PolicyEditor from "./routes/PolicyEditor";
import About from "./routes/About";

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
      {/* Opened from the user menu: the server's version and how it runs. */}
      <Route path="/about" component={About} />
      {/* Settings (git-bug 734ffa1), two separate pages: Account settings
          under /settings/<section>, an org's settings under
          /orgs/<org>/settings/<section>. Static first segments, so they
          outrank the /:org/:repo tier. */}
      <Route path="/settings" component={LegacySettings} />
      <Route path="/settings/:section" component={AccountSettings} />
      {/* The org page: its counts and its repositories, enabled or not.
          Under the static /orgs segment like the org's settings, so it
          cannot collide with /:org/:repo (the server refuses an org named
          `orgs`). The static /orgs/_new below outranks this :org route. */}
      <Route path="/orgs/:org" component={OrgPage} />
      <Route path="/orgs/:org/settings/:section?" component={OrgSettings} />
      {/* A new org's first forge connection, for global admins. `_new` holds
          the org's place in the path, and no org may be named with a
          leading `_`, so it shadows no org. */}
      <Route path="/orgs/_new" component={NewOrgSettings} />
      {/* Install or upgrade a library: its own URL inside the settings
          frame, so the review can be sent to a second reviewer. */}
      <Route path="/orgs/:org/settings/libraries/review" component={LibraryReview} />
      {/* One installed library or plugin, managed on its own page inside the
          settings frame. Six segments with static `libraries`/`plugins` and
          `manage`, so they collide with no other route; a library is
          `<publisher>~<name>`, a plugin installation its id. */}
      <Route path="/orgs/:org/settings/libraries/:id/manage" component={LibraryManage} />
      <Route path="/orgs/:org/settings/plugins/:id/manage" component={PluginManage} />
      {/* One forge connection, inside the settings frame with Connections
          marked. Five segments with a static `connections`, so it collides
          with neither the settings sections (four) nor another section's
          page; a connection id is one path segment. */}
      <Route path="/orgs/:org/settings/connections/:id" component={ConnectionManage} />
      {/* One member, by the Principal's id: a URL never carries a handle. */}
      <Route path="/orgs/:org/settings/members/:id/manage" component={MemberManage} />
      {/* One Environment (git-bug c11abc5): its manage page inside the
          settings frame. Six segments with a static last one, so it collides
          with neither the settings sections nor /:org/:repo/runs/:id; the
          name is one encoded segment. The former read-only page and edit
          screen redirect to it. */}
      <Route path="/orgs/:org/settings/environments/:env/manage" component={EnvironmentManage} />
      <Route path="/orgs/:org/settings/environments/:env" component={EnvironmentRedirect} />
      <Route path="/orgs/:org/settings/environments/:env/edit" component={EnvironmentRedirect} />
      {/* One org Policy, or `new`: its editor and history, inside the
          settings frame with Policies marked. */}
      <Route path="/orgs/:org/settings/policies/:id" component={PolicyEditor} />
      {/* Old addresses, kept for bookmarks and links sent before the move. */}
      <Route path="/settings/libraries/review" component={LegacySettings} />
      {/* The former Server administration page: now New organization. */}
      <Route path="/settings/server/:section?" component={LegacySettings} />
      <Route path="/account" component={LegacySettings} />
      <Route path="/account/security" component={LegacySettings} />
      {/* An org's former Event log section, now part of Security. A static
          last segment, so it outranks /orgs/:org/settings/:section. */}
      <Route path="/orgs/:org/settings/events" component={LegacySettings} />
      {/* A machine's StepUpConfirmationRequest: the
          `confirmation_url` its human opens. Three segments, so it can't
          collide with the two-segment /:org/:repo tier. */}
      <Route path="/security/confirm/:id" component={ConfirmRequest} />
      {/* Start a Run of a pipeline in this repository. */}
      <Route path="/:org/:repo/run" component={RunPipeline} />
      <Route path="/:org/:repo/runs/:id" component={RunDetail} />
      <Route path="/:org/:repo" component={RepoView} />
    </Router>
    </Show>
  );
}
