// The dogfooded API client — typed end-to-end against the generated OpenAPI
// schema (ADR-0012, 0028). The UI eats the same API as every other client.
import createClient from "openapi-fetch";
import { createSignal } from "solid-js";
import type { paths, components } from "./schema";
import type { ParamSpec } from "../params";
import { askStepUp, errorMessage, shareCeremony, withStepUp } from "../step-up";

export type RunStatus = components["schemas"]["RunStatusResponse"];
export type CreateRunRequest = components["schemas"]["CreateRunRequest"];
export type RunSummary = components["schemas"]["RunSummaryDto"];
export type CatalogEntry = components["schemas"]["CatalogEntry"];
export type DispatchKind = components["schemas"]["DispatchKind"];
export type Project = components["schemas"]["ProjectDto"];
export type Artifact = components["schemas"]["ArtifactDto"];
export type StepResult = components["schemas"]["StepResultDto"];
export type WorkspaceListing = components["schemas"]["WorkspaceListing"];
export type WorkspaceEntry = components["schemas"]["WorkspaceEntryDto"];
/** One attempt at a step (ADR-0047) — the rerun/retry unit. */
export type Attempt = components["schemas"]["AttemptDto"];
/** One step's status projection in a run's DAG. */
export type StepStatus = components["schemas"]["StepStatusDto"];
/** A run-scoped shared service instance (ADR-0058) — not a DAG node. */
export type Service = components["schemas"]["ServiceStatusDto"];
/** The cold tier's Workspace-Snapshot retention promise + any pin (ADR-0061 s5). */
export type RetentionInfo = components["schemas"]["SnapshotRetentionDto"];
/** A rerun preview: the resolved scope, and any widening over expired inputs. */
export type RerunPlan = components["schemas"]["RerunPlanResponse"];

export const api = createClient<paths>({ baseUrl: "/" });

/**
 * Is there no usable session? (ADR-0049)
 *
 * Set by the 401 middleware below and read by the app shell, which swaps the
 * whole UI for the sign-in screen. It lives here rather than in `session.ts`
 * because the *client* is what learns the answer — every call goes through this
 * middleware, so a session that expires mid-visit surfaces on the next request
 * instead of leaving the page throwing "failed to load" at the user.
 */
const [unauthenticated, setUnauthenticated] = createSignal(false);
export { unauthenticated };

// A 401 means "no session", and until this existed the UI had no answer for it:
// `GET /v1/me` failed, the identity menu rendered blank, every page threw its
// own "failed to load …", and there was no way to sign in from anywhere in the
// app. Only reachable with real auth configured, so dev-insecure deployments
// (every local loop) never saw it.
api.use({
  onResponse({ response }) {
    if (response.status === 401) {
      setUnauthenticated(true);
    }
    return response;
  },
});

// CSRF double-submit (ADR-0049): the session rides as an HttpOnly cookie; the
// server pairs it with a script-READABLE `scarab_csrf` cookie whose value we
// echo in `x-csrf-token`. A cross-site page can trigger the cookie, but it can
// never read this token.
//
// On EVERY request, not just mutations. The server gates the check on the
// authorized ACTION, not the HTTP method — `authenticate()` demands the token
// whenever `action != Action::Read` on a cookie-authenticated request — and
// several endpoints are GETs that authorize as `Administer`:
// `GET /v1/secrets` and `GET /v1/connections` are the two the Settings page
// needs. Sending the token only on mutations left that whole page 403ing with
// nothing to explain it, while every Action::Read endpoint worked.
//
// Sending it on safe methods costs nothing and loses nothing: a cross-site page
// still cannot set a custom header (it would need a CORS preflight the server
// does not grant), so the double-submit property is unchanged.
api.use({
  onRequest({ request }) {
    const csrf = document.cookie
      .split(";")
      .map((c) => c.trim())
      .find((c) => c.startsWith("scarab_csrf="))
      ?.slice("scarab_csrf=".length);
    if (csrf) {
      request.headers.set("x-csrf-token", csrf);
    }
    return request;
  },
});

// Step-up confirmation (ADR-0073). A crown-jewel operation answers
// `403 step_up_required` until this browser session holds a fresh passkey
// confirmation; this turns that 403 into the StepUpDialog, a passkey prompt, a
// `POST /v1/me/step-up`, and the SAME request again — so no call site in the
// app has to know step-up exists.
//
// Registered LAST, which matters twice over:
// - its `onRequest` runs after the CSRF middleware above, so the copy it
//   stashes already carries `x-csrf-token` and the retry passes the same
//   double-submit check the original did;
// - openapi-fetch runs `onResponse` in REVERSE order, so this one sees the 403
//   first and the 401 middleware above sees whatever the retry answered.
//
// The copy is taken before sending because a sent Request's body is spent: the
// retry needs a fresh clone of the original, once per round. A WeakMap keyed by
// the request object frees each stash with its request.
const stashedRequests = new WeakMap<Request, Request>();
/** One dialog at a time, shared by every request refused while it is open. */
const stepUpCeremony = shareCeremony(askStepUp);

api.use({
  onRequest({ request }) {
    stashedRequests.set(request, request.clone());
    return undefined;
  },
  async onResponse({ request, response }) {
    if (response.status !== 403) return response;
    const original = stashedRequests.get(request);
    if (!original) return response;
    return withStepUp(response, () => fetch(original.clone()), stepUpCeremony);
  },
});

/** List recent runs, newest first (dogfoods `GET /v1/runs`). */
export async function listRuns(limit = 50): Promise<RunSummary[]> {
  const { data, error } = await api.GET("/v1/runs", {
    params: { query: { limit } },
  });
  if (error || !data) {
    throw new Error("failed to list runs");
  }
  return data.runs;
}

/** List the registered projects (`GET /v1/repos`, ADR-0046) — the dashboard's
 * repo cards, most-recently-active first. Scoped server-side to what the caller
 * may Read. */
export async function listProjects(): Promise<Project[]> {
  const { data, error } = await api.GET("/v1/repos");
  if (error || !data) {
    throw new Error("failed to list projects");
  }
  return data;
}

/** The forge web base for a repo (e.g. `https://github.com/owner/name`), from
 * the project registry — for building commit/PR deep links. `null` if the repo
 * isn't in the registry or has no connection. */
export async function repoForgeUrl(org: string, repo: string): Promise<string | null> {
  const projects = await listProjects().catch(() => [] as Project[]);
  return projects.find((p) => p.org === org && p.project === repo)?.repo_url ?? null;
}

/** One repo's most recent runs (`GET /v1/repos/{org}/{repo}/runs`) — the source
 * for a repo card's pass/fail chart and the Runs tab. Newest first. `filter`
 * narrows by Environment and by the deploy marker on the server, before
 * `limit` (ADR-0074 §2); build it with `runListFilter` (runs-filter.ts). */
export async function listRepoRuns(
  org: string,
  repo: string,
  limit = 20,
  filter: { environment?: string; deploy?: boolean } = {},
): Promise<RunSummary[]> {
  const { data, error } = await api.GET("/v1/repos/{org}/{repo}/runs", {
    params: { path: { org, repo }, query: { limit, ...filter } },
  });
  if (error || !data) {
    throw new Error(`failed to list runs for ${org}/${repo}`);
  }
  return data.runs;
}

/** The authenticated principal (`GET /v1/me`, ADR-0049) — the identity menu. */
export type Me = components["schemas"]["MeResponse"];
export async function getMe(): Promise<Me> {
  const { data, error } = await api.GET("/v1/me");
  if (error || !data) {
    throw new Error("failed to load identity");
  }
  return data;
}

/** End the session (`POST /v1/auth/logout`), then send the user to login. */
export async function logout(): Promise<void> {
  await api.POST("/v1/auth/logout");
  // The doc comment above promised this for a while and the code did not do it:
  // the POST cleared the cookie server-side and the user was left looking at a
  // shell still rendering their name. Flip the shell over deliberately rather
  // than waiting for the next request to 401.
  setUnauthenticated(true);
}

// --- Issued API tokens (ADR-0049) — the credential a machine can hold. ---
//
// The plaintext exists in exactly ONE response, the mint 201, and nowhere else.
// Scarab stores only its SHA-256, so a token that is not copied out of the
// dialog that reveals it is re-minted, never re-read: there is deliberately no
// call here that can fetch an existing token's secret, because the server has
// nothing to answer with.
//
// A record's `role` is the granted CEILING, not the effective role — what the
// token may actually do is that ceiling ∩ what its owner holds at the moment of
// each request, so display it as "up to" and never as a promise.

/** One issued token's record — never the secret. */
export type ApiToken = components["schemas"]["ApiTokenDto"];
/** The mint response: the plaintext, once, plus the record it just created. */
export type MintedToken = components["schemas"]["MintedTokenDto"];
/** The mint body — name, ceiling role and lifetime are all required. */
export type MintTokenRequest = components["schemas"]["MintTokenRequest"];

/**
 * The tokens issued in an org (`GET /v1/orgs/{org}/tokens`), org- and
 * project-scoped alike.
 *
 * Returns `null` when the deployment has no token store — every token endpoint
 * answers 404 before it authorizes anything, so that is "this server cannot
 * issue tokens", not "you have none" and not a failure. The caller renders the
 * difference; collapsing the three would tell an admin their tokens vanished.
 */
export async function listApiTokens(org: string): Promise<ApiToken[] | null> {
  const { data, error, response } = await api.GET("/v1/orgs/{org}/tokens", {
    params: { path: { org } },
  });
  if (response.status === 404) return null;
  if (error || !data) {
    throw new Error(errorText(error) || "failed to load API tokens");
  }
  return data;
}

/**
 * Mint a token (`POST /v1/orgs/{org}/tokens`). The 201 body carries the
 * plaintext — the only time it exists outside the caller's hands.
 *
 * The 400s are worth surfacing verbatim rather than flattening to "invalid":
 * the server explains *which* ceiling was exceeded and what the minter actually
 * holds, which is the whole answer to "why can I not mint this".
 */
export async function mintApiToken(
  org: string,
  body: MintTokenRequest,
): Promise<MintedToken> {
  const { data, error, response } = await api.POST("/v1/orgs/{org}/tokens", {
    params: { path: { org } },
    body,
  });
  if (error || !data) {
    throw new Error(errorText(error) || `could not mint the token (${response.status})`);
  }
  return data;
}

/**
 * Revoke a token (`DELETE /v1/orgs/{org}/tokens/{id}`) — permanent, and
 * effective on the very next request that presents it. `id` is the record's id,
 * never the credential.
 */
export async function revokeApiToken(org: string, id: string): Promise<void> {
  const { error, response } = await api.DELETE("/v1/orgs/{org}/tokens/{id}", {
    params: { path: { org, id } },
  });
  if (error) {
    throw new Error(errorText(error) || `could not revoke the token (${response.status})`);
  }
}

// --- Identity links (ADR-0069) — sign-in accounts, members, imports. ---

/** A configured login provider, for the sign-in page. */
export type AuthProvider = components["schemas"]["AuthProviderDto"];
/** A user, as the grant/approver typeahead offers them. */
export type PrincipalMatch = components["schemas"]["PrincipalDto"];
/** One role binding — enriched with the person's name and whether they have
 * signed in yet. */
export type Binding = components["schemas"]["BindingDto"];

/** The `scarab_csrf` double-submit token (ADR-0049), for the few calls made
 * with plain `fetch` rather than through `api` (whose middleware adds it). */
function csrfToken(): string | undefined {
  return document.cookie
    .split(";")
    .map((c) => c.trim())
    .find((c) => c.startsWith("scarab_csrf="))
    ?.slice("scarab_csrf=".length);
}

/**
 * `fetch` with the CSRF header and a JSON body, throwing the SERVER's own
 * message on failure. Several of these endpoints refuse with an explanation
 * worth showing verbatim — "`github:bob` is not a Scarab user…", "no login
 * provider is associated with the forge at …" — and a generic "failed" would
 * hide the one thing the admin needs to know.
 */
async function send(path: string, method: string, body?: unknown): Promise<Response> {
  const headers: Record<string, string> = { "content-type": "application/json" };
  const csrf = csrfToken();
  if (csrf) headers["x-csrf-token"] = csrf;
  const init: RequestInit = {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  };
  // Several of these are crown jewels (role bindings, identity links): the
  // same step-up retry the `api` middleware does. The body is a string, so
  // re-sending the same init is the same request.
  const res = await withStepUp(await fetch(path, init), () => fetch(path, init), stepUpCeremony);
  if (res.status === 401) setUnauthenticated(true);
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(errorMessage(text, `${method} ${path} failed (${res.status})`));
  }
  return res;
}

/** The configured login providers (`GET /v1/auth/providers`) — readable before
 * sign-in. Best-effort: an older server without the route yields `[]`, which
 * the sign-in page treats as "one provider, the default". */
export async function listAuthProviders(): Promise<AuthProvider[]> {
  const res = await fetch("/v1/auth/providers").catch(() => null);
  if (!res || !res.ok) return [];
  return (await res.json()) as AuthProvider[];
}

/** Start linking another provider's account (`POST /v1/me/identities/link`):
 * the browser is then sent to the returned authorize URL, and the callback
 * lands back on `/account` with the outcome in the query string. */
export async function beginLinkIdentity(provider: string): Promise<string> {
  const res = await send("/v1/me/identities/link", "POST", { provider });
  const body = (await res.json()) as components["schemas"]["LinkIdentityResponse"];
  return body.authorize_url;
}

/** Unlink this person's account on `provider` (`DELETE /v1/me/identities/…`);
 * the roles imported through it go with it. */
export async function unlinkIdentity(provider: string): Promise<void> {
  await send(`/v1/me/identities/${encodeURIComponent(provider)}`, "DELETE");
}

/** An org's role bindings (`GET /v1/orgs/{org}/bindings`); `null` when the
 * deployment has no RBAC store (dev). */
export async function listBindings(org: string): Promise<Binding[] | null> {
  const res = await fetch(`/v1/orgs/${encodeURIComponent(org)}/bindings`, {
    headers: csrfToken() ? { "x-csrf-token": csrfToken()! } : {},
  });
  if (res.status === 404) return null;
  if (res.status === 401) setUnauthenticated(true);
  if (!res.ok) throw new Error((await res.text()).trim() || "failed to load members");
  return (await res.json()) as Binding[];
}

/** Natively grant `subject` a role, org-wide or on one project. The subject
 * must be an existing user (ADR-0069 §3) — pick it from `searchPrincipals`. */
export async function grantRole(
  org: string,
  body: { subject: string; role: string; project?: string },
): Promise<void> {
  await send(`/v1/orgs/${encodeURIComponent(org)}/bindings`, "PUT", body);
}

/** Natively revoke — a durable tombstone a later forge re-sync cannot undo. */
export async function revokeRole(org: string, subject: string, project?: string): Promise<void> {
  const q = new URLSearchParams({ subject });
  if (project) q.set("project", project);
  await send(`/v1/orgs/${encodeURIComponent(org)}/bindings?${q}`, "DELETE");
}

/** Re-sync a repo's forge collaborators into roles (ADR-0069 §5): everyone the
 * forge lists is seeded (pinned by their forge id, so whoever signs in with
 * that account gets exactly these roles), and anyone it no longer lists loses
 * the role the forge gave them. */
export async function importFromForge(org: string, repo: string): Promise<Binding[]> {
  const res = await send(
    `/v1/repos/${encodeURIComponent(org)}/${encodeURIComponent(repo)}/bindings/import`,
    "POST",
    {},
  );
  return (await res.json()) as Binding[];
}

/** Find users for a grant or an approver entry (`GET /v1/orgs/{org}/principals`). */
export async function searchPrincipals(org: string, q: string): Promise<PrincipalMatch[]> {
  if (!q.trim()) return [];
  const res = await send(
    `/v1/orgs/${encodeURIComponent(org)}/principals?q=${encodeURIComponent(q.trim())}`,
    "GET",
  );
  return (await res.json()) as PrincipalMatch[];
}

/** List a run's artifacts of record (`GET /v1/runs/{id}/artifacts`, ADR-0052). */
export async function listArtifacts(id: string): Promise<Artifact[]> {
  const { data, error } = await api.GET("/v1/runs/{id}/artifacts", {
    params: { path: { id } },
  });
  if (error || !data) {
    throw new Error(`failed to list artifacts for ${id}`);
  }
  return data;
}

/** Browser URL for one artifact's bytes (`GET /v1/runs/{id}/artifacts/{name}`,
 * streamed through the server — usable directly as an `<a href>` download).
 * Bare name = the of-record resolution (latest SUCCESSFUL version, ADR-0056);
 * `version` pins the exact `(step, attempt)` version — how a shadowed or
 * failed-attempt file stays reachable. */
export function artifactUrl(
  id: string,
  name: string,
  version?: { step: string; attempt: string },
): string {
  const base = `/v1/runs/${encodeURIComponent(id)}/artifacts/${encodeURIComponent(name)}`;
  return version
    ? `${base}?step=${encodeURIComponent(version.step)}&attempt=${encodeURIComponent(version.attempt)}`
    : base;
}

// --- Run detail Inspector: a step's browseable outputs. Results are the typed
// values a step published (ADR-0041); the workspace is its output snapshot,
// walked read-only from the content-addressed store (ADR-0029). ---

/** A step's named results (`GET …/steps/{step}/results`). Empty if it emitted
 * none. The Results tab, and the source the Outputs view derives from. With
 * `attempt`, that attempt's immutable evidence instead of the latest (ADR-0056). */
export async function getStepResults(
  id: string,
  step: string,
  attempt?: string,
): Promise<StepResult[]> {
  const { data, error } = await api.GET("/v1/runs/{id}/steps/{step}/results", {
    params: { path: { id, step }, query: attempt ? { attempt } : {} },
  });
  if (error || !data) {
    throw new Error(`failed to load results for ${step}`);
  }
  return data;
}

/** What an attempt consumed (`GET …/steps/{step}/consumed`, ADR-0056): the map
 * `upstream step → attempt id` stamped at its launch. Bare = the attempt behind
 * the step's current evidence. Empty map when nothing was recorded. */
export async function getConsumed(
  id: string,
  step: string,
  attempt?: string,
): Promise<{ attempt: string; consumed: Record<string, string> }> {
  const { data, error } = await api.GET("/v1/runs/{id}/steps/{step}/consumed", {
    params: { path: { id, step }, query: attempt ? { attempt } : {} },
  });
  if (error || !data) {
    throw new Error(`failed to load consumption for ${step}`);
  }
  return data as { attempt: string; consumed: Record<string, string> };
}

/** List a directory in a step's output workspace snapshot
 * (`GET …/steps/{step}/workspace?path=`). `available:false` when the step
 * produced no snapshot (still running, a gate, or a non-snapshotting backend). */
export async function listWorkspace(
  id: string,
  step: string,
  path = "",
  attempt?: string,
): Promise<WorkspaceListing> {
  const { data, error } = await api.GET("/v1/runs/{id}/steps/{step}/workspace", {
    params: { path: { id, step }, query: attempt ? { path, attempt } : { path } },
  });
  if (error || !data) {
    throw new Error(`failed to browse workspace for ${step}`);
  }
  return data;
}

/** Browser URL for one workspace file's bytes (`GET …/steps/{step}/workspace/file?path=`,
 * streamed through the server — usable directly as an `<a href>`). With
 * `attempt`, reads that attempt's immutable snapshot (ADR-0056). */
export function workspaceFileUrl(
  id: string,
  step: string,
  path: string,
  attempt?: string,
): string {
  const base =
    `/v1/runs/${encodeURIComponent(id)}/steps/${encodeURIComponent(step)}` +
    `/workspace/file?path=${encodeURIComponent(path)}`;
  return attempt ? `${base}&attempt=${encodeURIComponent(attempt)}` : base;
}

// --- A repository's Environments (ADR-0071). Since the migration an
// Environment is org-level; a repository sees the ones its Project may deploy
// into (or holds an overlay or grant in), each with the org's BASELINE, the
// Project's own tighten-only OVERLAY, and the EFFECTIVE rules admission
// enforces — merged by the server, never here. The old per-repo
// `PUT/DELETE …/environments/{name}` answer 410. ---

/** One Environment as this Project sees it. */
export type ProjectEnvironment = components["schemas"]["ProjectEnvironmentDto"];
/** One rules layer (a baseline or an overlay's rules). */
export type EnvironmentRules = components["schemas"]["EnvironmentProtectionRulesDto"];
/** The merged rules admission enforces. */
export type EffectiveRules = components["schemas"]["EffectiveProtectionDto"];
/** A Project's overlay as stored. */
export type EnvironmentOverlay = components["schemas"]["EnvironmentProjectOverlayDto"];
/** The overlay PUT body. */
export type PutOverlayRequest = components["schemas"]["PutEnvironmentProjectOverlayRequest"];

/** The Environments this repository's Project sees (`GET …/environments`). */
export async function listEnvironments(org: string, repo: string): Promise<ProjectEnvironment[]> {
  // Plain fetch: the spec declares this route's response but not its path
  // parameters, so the typed client cannot address it.
  const res = await fetch(
    `/v1/repos/${encodeURIComponent(org)}/${encodeURIComponent(repo)}/environments`,
  );
  if (res.status === 401) setUnauthenticated(true);
  if (!res.ok) throw new Error(`failed to list environments (${res.status})`);
  return (await res.json()) as ProjectEnvironment[];
}

const envPath = (org: string, env: string) =>
  `/v1/orgs/${encodeURIComponent(org)}/environments/${encodeURIComponent(env)}`;
const projectPath = (org: string, env: string, project: string) =>
  `${envPath(org, env)}/projects/${encodeURIComponent(project)}`;

/** Tighten an Environment for one Project — create or replace its overlay
 *  (Administer on the Project or Org). Through `send`: the server's 400
 *  (an approver who is not a Scarab user) and 409 (not eligible) are the
 *  answer to "why will this not save", shown verbatim. */
export async function putEnvironmentOverlay(
  org: string,
  env: string,
  project: string,
  body: PutOverlayRequest,
): Promise<EnvironmentOverlay> {
  const res = await send(`${projectPath(org, env, project)}/overlay`, "PUT", body);
  return (await res.json()) as EnvironmentOverlay;
}

/** Remove a Project's overlay: its deploys fall back to the baseline. */
export async function deleteEnvironmentOverlay(
  org: string,
  env: string,
  project: string,
): Promise<void> {
  await send(`${projectPath(org, env, project)}/overlay`, "DELETE");
}

/** One approval recorded when a run into an environment was admitted (`at` is
 *  epoch millis). The LEGACY `…/environments/{name}/deployments` route: it
 *  records what was *approved*, not what went live. */
export type ApprovalRecord = {
  org: string;
  project: string;
  environment: string;
  git_ref: string;
  run: string;
  approved_by: string[];
  at: number;
};

/** An environment's approval history, most recent first (legacy
 *  `GET /v1/repos/{org}/{repo}/environments/{name}/deployments`). */
export async function listApprovalHistory(
  org: string,
  repo: string,
  name: string,
): Promise<ApprovalRecord[]> {
  const res = await fetch(
    `/v1/repos/${encodeURIComponent(org)}/${encodeURIComponent(repo)}/environments/${encodeURIComponent(name)}/deployments`,
  );
  if (!res.ok) throw new Error("failed to list approval history");
  return (await res.json()) as ApprovalRecord[];
}

// --- Org-level Environments (ADR-0071) — the place deploys land in: its
// description (the whole-Environment PUT), and — each with its own write, so
// that PUT can never reset them — its baseline protection, its eligibility,
// and each Project's privilege grant. Org admins only. ---

/** An org-level Environment. */
export type OrgEnvironment = components["schemas"]["OrgEnvironmentDto"];
/** The PUT body: the whole Environment, replaced. */
export type PutOrgEnvironmentRequest = components["schemas"]["PutOrgEnvironmentRequest"];

/** The caller may not read this org's Environments (403): reading needs Read
 *  on the Org. */
export class OrgEnvironmentsForbiddenError extends Error {
  constructor(readonly org: string) {
    super(`no read access to ${org}'s environments`);
    this.name = "OrgEnvironmentsForbiddenError";
  }
}

/**
 * The org's Environments, by name (`GET /v1/orgs/{org}/environments`).
 *
 * `null` when the server has no org-Environment store (a 404, answered once
 * the caller is authorized) — "this server has no org Environments", not
 * "none registered".
 */
export async function listOrgEnvironments(org: string): Promise<OrgEnvironment[] | null> {
  const { data, error, response } = await api.GET("/v1/orgs/{org}/environments", {
    params: { path: { org } },
  });
  if (response.status === 404) return null;
  if (response.status === 403) throw new OrgEnvironmentsForbiddenError(org);
  if (error || !data) {
    throw new Error(errorText(error) || "failed to load the org's environments");
  }
  return data;
}

/**
 * Create or replace an Environment (`PUT /v1/orgs/{org}/environments/{env}`),
 * org admins only. A full replace: send every field. The server's 400 is
 * passed through verbatim — it is the answer to "why will this not save".
 */
export async function putOrgEnvironment(
  org: string,
  env: string,
  body: PutOrgEnvironmentRequest,
): Promise<OrgEnvironment> {
  const { data, error, response } = await api.PUT("/v1/orgs/{org}/environments/{env}", {
    params: { path: { org, env } },
    body,
  });
  if (error || !data) {
    throw new Error(errorText(error) || `could not save ${env} (${response.status})`);
  }
  return data;
}

/** Delete an Environment (`DELETE /v1/orgs/{org}/environments/{env}`), org
 *  admins only. */
export async function deleteOrgEnvironment(org: string, env: string): Promise<void> {
  const { error, response } = await api.DELETE("/v1/orgs/{org}/environments/{env}", {
    params: { path: { org, env } },
  });
  if (error) {
    throw new Error(errorText(error) || `could not delete ${env} (${response.status})`);
  }
}

/** The baseline PUT body. */
export type PutProtectionRequest = components["schemas"]["PutEnvironmentProtectionRequest"];
/** Which Projects may deploy into an Environment. */
export type EnvironmentEligibility = components["schemas"]["EnvironmentEligibilityDto"];
/** A Project's standing in one Environment: eligible, overlay, grant. */
export type EnvironmentProject = components["schemas"]["EnvironmentProjectDto"];
/** A Project's privileges in one Environment. */
export type PrivilegeGrant = components["schemas"]["EnvironmentProjectPrivilegeGrantDto"];
export type PutPrivilegeGrantRequest =
  components["schemas"]["PutEnvironmentProjectPrivilegeGrantRequest"];
/** One line of the migration's split report. */
export type MigrationReportLine = components["schemas"]["EnvironmentMigrationReportDto"];

/** Replace an Environment's baseline. Returns the Environment as stored. */
export async function putEnvironmentProtection(
  org: string,
  env: string,
  body: PutProtectionRequest,
): Promise<OrgEnvironment> {
  const res = await send(`${envPath(org, env)}/protection`, "PUT", body);
  return (await res.json()) as OrgEnvironment;
}

/** Replace an Environment's eligibility. */
export async function putEnvironmentEligibility(
  org: string,
  env: string,
  body: EnvironmentEligibility,
): Promise<EnvironmentEligibility> {
  const res = await send(`${envPath(org, env)}/eligibility`, "PUT", body);
  return (await res.json()) as EnvironmentEligibility;
}

/** Every Project holding an overlay or grant in an Environment. */
export async function listEnvironmentProjects(
  org: string,
  env: string,
): Promise<EnvironmentProject[]> {
  const { data, error, response } = await api.GET("/v1/orgs/{org}/environments/{env}/projects", {
    params: { path: { org, env } },
  });
  if (error || !data) {
    throw new Error(errorText(error) || `failed to list ${env}'s projects (${response.status})`);
  }
  return data;
}

/** Grant a Project privileges in an Environment (replaces any grant). */
export async function putPrivilegeGrant(
  org: string,
  env: string,
  project: string,
  body: PutPrivilegeGrantRequest,
): Promise<PrivilegeGrant> {
  const res = await send(`${projectPath(org, env, project)}/privilege-grant`, "PUT", body);
  return (await res.json()) as PrivilegeGrant;
}

/** Withdraw a Project's privilege grant. */
export async function deletePrivilegeGrant(org: string, env: string, project: string): Promise<void> {
  await send(`${projectPath(org, env, project)}/privilege-grant`, "DELETE");
}

/** The migration's split report. `null` when the caller may not read it
 *  (403 — it needs Administer on the org) or the server has no governance
 *  store (404): the panel then simply does not show. */
export async function getMigrationReport(org: string): Promise<MigrationReportLine[] | null> {
  const { data, response } = await api.GET("/v1/orgs/{org}/environment-migration-report", {
    params: { path: { org } },
  });
  if (response.status === 403 || response.status === 404) return null;
  if (!data) throw new Error(`failed to load the migration report (${response.status})`);
  return data;
}

/** Mark one split-report line reviewed (the first acknowledgement stands). */
export async function acknowledgeMigrationReport(
  org: string,
  env: string,
): Promise<MigrationReportLine> {
  const res = await send(
    `/v1/orgs/${encodeURIComponent(org)}/environment-migration-report/${encodeURIComponent(env)}/acknowledge`,
    "POST",
  );
  return (await res.json()) as MigrationReportLine;
}

// --- Plugins (ADR-0072) — an org's PluginInstallations:
// approvals of one pinned digest of a first-party plugin, with a kill switch.
// Every write's refusal text (403 third-party / 409 name taken or retired) is
// passed through verbatim — it is the answer to "why will this not install". ---

/** A plugin this Scarab can install (first-party only, today). */
export type PluginCatalogEntry = components["schemas"]["PluginCatalogEntryDto"];
/** An org's approval to run one pinned digest of a plugin. */
export type PluginInstallation = components["schemas"]["PluginInstallationDto"];
/** What an uninstall returns. */
export type UninstalledPlugin = components["schemas"]["UninstalledPluginDto"];

// --- Libraries (ADR-0076) ---------------------------------------------------

export type OrgLibraries = components["schemas"]["OrgLibrariesDto"];
export type LibraryInstallation = components["schemas"]["LibraryInstallationDto"];
export type FetchedLibrary = components["schemas"]["FetchedLibraryDto"];
export type LibraryDefaults = components["schemas"]["LibraryDefaultsDto"];

/** The org's libraries (`GET …/libraries`); `null` = this server does not
 *  manage libraries (404). */
export async function listLibraries(org: string): Promise<OrgLibraries | null> {
  const { data, error, response } = await api.GET("/v1/orgs/{org}/libraries", {
    params: { path: { org } },
  });
  if (response.status === 404) return null;
  if (error || !data) throw new Error(errorText(error) || "failed to load the org's libraries");
  return data;
}

/** Fetch a version to review (`POST …/libraries/fetch`): stored, not installed. */
export async function fetchLibrary(
  org: string,
  body: components["schemas"]["FetchLibraryRequest"],
): Promise<FetchedLibrary> {
  const { data, error, response } = await api.POST("/v1/orgs/{org}/libraries/fetch", {
    params: { path: { org } },
    body,
  });
  if (error || !data) throw new Error(errorText(error) || `could not fetch ${body.source} (${response.status})`);
  return data;
}

/** Install or upgrade to a fetched version (`PUT …/libraries/{publisher}/{name}`).
 *  Asks for a step-up. */
export async function installLibrary(
  org: string,
  publisher: string,
  name: string,
  body: components["schemas"]["InstallLibraryRequest"],
): Promise<LibraryInstallation> {
  const { data, error, response } = await api.PUT("/v1/orgs/{org}/libraries/{publisher}/{name}", {
    params: { path: { org, publisher, name } },
    body,
  });
  if (error || !data) {
    throw new Error(errorText(error) || `could not install lib:${publisher}/${name} (${response.status})`);
  }
  return data;
}

/** Remove an installation (`DELETE …/libraries/{publisher}/{name}`). */
export async function removeLibrary(org: string, publisher: string, name: string): Promise<void> {
  const { error, response } = await api.DELETE("/v1/orgs/{org}/libraries/{publisher}/{name}", {
    params: { path: { org, publisher, name } },
  });
  if (!response.ok) {
    throw new Error(errorText(error) || `could not remove lib:${publisher}/${name} (${response.status})`);
  }
}

export type YankImpact = components["schemas"]["YankImpactDto"];
export type LibraryYank = components["schemas"]["LibraryYankDto"];

/** The Runs in progress that use a version (`GET …/library-yanks/{hash}/impact`). */
export async function yankImpact(org: string, hash: string): Promise<YankImpact> {
  const { data, error, response } = await api.GET("/v1/orgs/{org}/library-yanks/{hash}/impact", {
    params: { path: { org, hash } },
  });
  if (error || !data) throw new Error(errorText(error) || `could not count the Runs (${response.status})`);
  return data;
}

/** Yank a version in this org (`POST …/library-yanks`); `cancel_runs` also
 *  cancels the Runs in progress that use it. */
export async function yankLibrary(
  org: string,
  content_hash: string,
  reason: string,
  cancel_runs = false,
): Promise<LibraryYank> {
  const { data, error, response } = await api.POST("/v1/orgs/{org}/library-yanks", {
    params: { path: { org } },
    body: { content_hash, reason, cancel_runs },
  });
  if (error || !data) throw new Error(errorText(error) || `could not yank the version (${response.status})`);
  return data;
}

/** Lift a yank (`DELETE …/library-yanks/{hash}`). Asks for a step-up. */
export async function unyankLibrary(org: string, hash: string): Promise<void> {
  const { error, response } = await api.DELETE("/v1/orgs/{org}/library-yanks/{hash}", {
    params: { path: { org, hash } },
  });
  if (!response.ok) throw new Error(errorText(error) || `could not lift the yank (${response.status})`);
}

/** Install the missing first-party libraries (`POST …/library-defaults/retry`),
 *  optionally from a mirror. Asks for a step-up. */
export async function retryLibraryDefaults(
  org: string,
  body: components["schemas"]["RetryDefaultsRequest"],
): Promise<LibraryDefaults> {
  const { data, error, response } = await api.POST("/v1/orgs/{org}/library-defaults/retry", {
    params: { path: { org } },
    body,
  });
  if (error || !data) throw new Error(errorText(error) || `could not install the libraries (${response.status})`);
  return data;
}

/** The installable plugins (`GET …/plugin-catalog`); `null` = this server
 *  runs no plugins (404). */
export async function listPluginCatalog(org: string): Promise<PluginCatalogEntry[] | null> {
  const { data, error, response } = await api.GET("/v1/orgs/{org}/plugin-catalog", {
    params: { path: { org } },
  });
  if (response.status === 404) return null;
  if (error || !data) throw new Error(errorText(error) || "failed to load the plugin catalog");
  return data;
}

/** The org's installations (`GET …/plugins`); `null` = plugins not
 *  configured on this server (404) — not "none installed". */
export async function listPlugins(org: string): Promise<PluginInstallation[] | null> {
  const { data, error, response } = await api.GET("/v1/orgs/{org}/plugins", {
    params: { path: { org } },
  });
  if (response.status === 404) return null;
  if (error || !data) throw new Error(errorText(error) || "failed to load the org's plugins");
  return data;
}

/** Install a first-party plugin pinned by digest (`POST …/plugins`). */
export async function installPlugin(
  org: string,
  body: components["schemas"]["InstallPluginRequest"],
): Promise<PluginInstallation> {
  const { data, error, response } = await api.POST("/v1/orgs/{org}/plugins", {
    params: { path: { org } },
    body,
  });
  if (error || !data) {
    throw new Error(errorText(error) || `could not install ${body.name} (${response.status})`);
  }
  return data;
}

/** Kill an installation at once (`POST …/plugins/{name}/kill`): its broker
 *  calls are refused from the next request on. */
export async function killPlugin(org: string, name: string): Promise<PluginInstallation> {
  const { data, error, response } = await api.POST("/v1/orgs/{org}/plugins/{name}/kill", {
    params: { path: { org, name } },
  });
  if (error || !data) throw new Error(errorText(error) || `could not kill ${name} (${response.status})`);
  return data;
}

/** Re-enable a killed installation (`POST …/plugins/{name}/enable`). */
export async function enablePlugin(org: string, name: string): Promise<PluginInstallation> {
  const { data, error, response } = await api.POST("/v1/orgs/{org}/plugins/{name}/enable", {
    params: { path: { org, name } },
  });
  if (error || !data) throw new Error(errorText(error) || `could not enable ${name} (${response.status})`);
  return data;
}

/** Uninstall (`DELETE …/plugins/{name}`). The name is retired for good. */
export async function uninstallPlugin(org: string, name: string): Promise<UninstalledPlugin> {
  const { data, error, response } = await api.DELETE("/v1/orgs/{org}/plugins/{name}", {
    params: { path: { org, name } },
  });
  if (error || !data) throw new Error(errorText(error) || `could not uninstall ${name} (${response.status})`);
  return data;
}

/** Mount an operator-made PersistentVolumeClaim into the installation's Pod
 *  (`PUT …/plugins/{name}/storage`, ADR-0074 §7). Moving existing storage to
 *  another claim or path asks for a step-up. */
export async function putPluginStorage(
  org: string,
  name: string,
  body: components["schemas"]["PluginStorageRequest"],
): Promise<PluginInstallation> {
  const { data, error, response } = await api.PUT("/v1/orgs/{org}/plugins/{name}/storage", {
    params: { path: { org, name } },
    body,
  });
  if (error || !data) {
    throw new Error(errorText(error) || `could not set ${name}'s storage (${response.status})`);
  }
  return data;
}

/** Stop mounting the installation's claim (`DELETE …/plugins/{name}/storage`).
 *  The claim and its data stay in the cluster. */
export async function deletePluginStorage(org: string, name: string): Promise<PluginInstallation> {
  const { data, error, response } = await api.DELETE("/v1/orgs/{org}/plugins/{name}/storage", {
    params: { path: { org, name } },
  });
  if (error || !data) {
    throw new Error(errorText(error) || `could not remove ${name}'s storage (${response.status})`);
  }
  return data;
}

/** One connection slot of an installation and what is bound to it — never
 *  the credential (ADR-0072 §5). */
export type PluginConnection = components["schemas"]["PluginConnectionDto"];

/** An installation's connection slots, in manifest order
 *  (`GET …/plugins/{name}/connections`). */
export async function listPluginConnections(org: string, name: string): Promise<PluginConnection[]> {
  const { data, error, response } = await api.GET("/v1/orgs/{org}/plugins/{name}/connections", {
    params: { path: { org, name } },
  });
  if (error || !data) {
    throw new Error(errorText(error) || `failed to load ${name}'s connections (${response.status})`);
  }
  return data;
}

/** Bind a slot's destination and credential, replacing the previous binding
 *  whole (`PUT …/connections/{slot}`). The credential is write-only. */
export async function putPluginConnection(
  org: string,
  name: string,
  slot: string,
  body: components["schemas"]["PutPluginConnectionRequest"],
): Promise<PluginConnection> {
  const { data, error, response } = await api.PUT("/v1/orgs/{org}/plugins/{name}/connections/{slot}", {
    params: { path: { org, name, slot } },
    body,
  });
  if (error || !data) {
    throw new Error(errorText(error) || `could not bind ${slot} (${response.status})`);
  }
  return data;
}

/** Unbind a slot and delete its credential (`DELETE …/connections/{slot}`). */
export async function deletePluginConnection(org: string, name: string, slot: string): Promise<void> {
  const { error, response } = await api.DELETE("/v1/orgs/{org}/plugins/{name}/connections/{slot}", {
    params: { path: { org, name, slot } },
  });
  if (error || !response.ok) {
    throw new Error(errorText(error) || `could not unbind ${slot} (${response.status})`);
  }
}

/** Fetch a run's status + steps (dogfoods `GET /v1/runs/{id}`). */
export async function getRun(id: string): Promise<RunStatus> {
  const { data, error } = await api.GET("/v1/runs/{id}", {
    params: { path: { id } },
  });
  if (error || !data) {
    throw new Error(`failed to load run ${id}`);
  }
  return data;
}

/** Create a run from an inline pipeline (dogfoods `POST /v1/runs`). */
export async function createRun(req: CreateRunRequest): Promise<string> {
  const { data, error } = await api.POST("/v1/runs", { body: req });
  if (error || !data) {
    throw new Error("failed to create run");
  }
  return data.id;
}

/** A prior run's frozen launch parameters, `name → value` (ADR-0043 §5) — used
 * to pre-fill a re-run. The generated schema types this opaquely; the values
 * are plain JSON scalars. Empty when the run took none. */
export function runParams(run: RunStatus): Record<string, unknown> {
  return (run.params ?? {}) as Record<string, unknown>;
}

// --- Run-Pipeline flow (ADR-0043 "World B"): repo → ref → catalog → pipeline →
// typed params → dispatch. One ref threads through; describe/catalog resolve it
// to a SHA the dispatch pins to. The UI eats the same endpoints the CLI does
// (invariant #5) — no client-side CEL, static validation only. ---

export type PipelineCatalog = { sha: string; pipelines: CatalogEntry[] };
export type PipelineInterface = {
  sha: string;
  manual: boolean;
  api: boolean;
  inputs: ParamSpec[];
};
/** Dispatch outcome: the new run id, or the server's fail-closed 4xx message
 * (parsed onto the offending field by the caller). No run is created on error. */
export type DispatchResult =
  | { ok: true; id: string }
  | { ok: false; status: number; message: string };

/** One branch or tag from the ref picker (`GET …/refs`). */
export type ForgeRef = components["schemas"]["RefDto"];

/** The repo's branches + tags for the ref picker (`GET …/refs?q=`), optionally
 * narrowed by a case-insensitive name substring. Branches sort before tags,
 * each group name-ascending, server-side. Returns `[]` on error so the picker
 * degrades to plain free-text entry rather than blocking dispatch. */
export async function listRefs(
  org: string,
  repo: string,
  q?: string,
): Promise<ForgeRef[]> {
  const { data, error } = await api.GET("/v1/repos/{org}/{repo}/refs", {
    params: { path: { org, repo }, query: q ? { q } : {} },
  });
  if (error || !data) return [];
  return data.refs;
}

/** The manually-dispatchable catalog at a ref (`GET …/pipelines?ref=`). */
export async function listPipelines(
  org: string,
  repo: string,
  ref: string,
): Promise<PipelineCatalog> {
  const { data, error } = await api.GET("/v1/repos/{org}/{repo}/pipelines", {
    params: { path: { org, repo }, query: { ref } },
  });
  if (error || !data) {
    throw new Error("failed to list pipelines");
  }
  return data as PipelineCatalog;
}

/** The compiled, typed parameter schema for one pipeline at a ref
 * (`GET …/pipelines/{name}/interface?ref=`). `inputs` is served opaquely, so we
 * assert it to the hand-authored `ParamSpec` shape. */
export async function pipelineInterface(
  org: string,
  repo: string,
  name: string,
  ref: string,
): Promise<PipelineInterface> {
  const { data, error, response } = await api.GET(
    "/v1/repos/{org}/{repo}/pipelines/{name}/interface",
    { params: { path: { org, repo, name }, query: { ref } } },
  );
  if (response.status === 400 && error && typeof error === "object" && "errors" in error) {
    throw new PipelineCompileError(error as unknown as CompileFailure);
  }
  if (error || !data) {
    throw new Error(
      response.status === 400
        ? `pipeline failed to compile: ${errorText(error)}`
        : response.status === 404
          ? `no pipeline "${name}" at ${ref}`
          : "failed to load pipeline interface",
    );
  }
  // `inputs` is served opaquely (`Record<string, never>[]`); reinterpret it as
  // the hand-authored ParamSpec shape via `unknown`.
  return data as unknown as PipelineInterface;
}

/** Dispatch a named pipeline at a ref (`POST …/dispatch`). Returns a result
 * union rather than throwing, so a fail-closed 4xx maps onto the form. */
export async function dispatchRun(
  org: string,
  repo: string,
  body: {
    ref: string;
    pipeline: string;
    params: Record<string, unknown>;
    kind?: DispatchKind;
    /** Optional operator reason (ADR-0057) — stamped as the run Headline. */
    reason?: string;
  },
): Promise<DispatchResult> {
  const { data, error, response } = await api.POST("/v1/repos/{org}/{repo}/dispatch", {
    params: { path: { org, repo } },
    // The generated body types `params` opaquely (`Record<string, never>`); our
    // typed map is the intended JSON payload.
    body: body as never,
  });
  if (data) return { ok: true, id: data.id };
  return { ok: false, status: response.status, message: errorText(error) };
}

export type CompileError = components["schemas"]["CompileErrorDto"];
export type CompileFailure = components["schemas"]["CompileErrorResponse"];

/** Thrown by [`pipelineInterface`] when the Pipeline does not compile: the
 *  server's errors, each with its fields. */
export class PipelineCompileError extends Error {
  constructor(readonly failure: CompileFailure) {
    super(`pipeline failed to compile: ${failure.message}`);
  }
}

export type RefusedRun = components["schemas"]["RefusedRunDto"];

/** A Project's triggers that did not become a Run (`GET …/refused-runs`). */
export async function listRefusedRuns(org: string, repo: string, limit = 50): Promise<RefusedRun[]> {
  const { data, error } = await api.GET("/v1/repos/{org}/{repo}/refused-runs", {
    params: { path: { org, repo }, query: { limit } },
  });
  if (error || !data) throw new Error(`failed to list refused runs for ${org}/${repo}`);
  return data.refused;
}

export type StepOrigin = components["schemas"]["StepOriginDto"];

/** Where a Step comes from (`GET /v1/runs/{id}/steps/{step}/origin`). */
export async function stepOrigin(run: string, step: string): Promise<StepOrigin> {
  const { data, error, response } = await api.GET("/v1/runs/{id}/steps/{step}/origin", {
    params: { path: { id: run, step } },
  });
  if (error || !data) throw new Error(errorText(error) || `could not load where ${step} comes from (${response.status})`);
  return data;
}

/** Best-effort extraction of an error body to text. The dispatch/interface 4xx
 * bodies are plain text; openapi-fetch surfaces them in `error` (string), but a
 * stray JSON body is stringified rather than dropped. */
function errorText(error: unknown): string {
  if (error == null) return "";
  if (typeof error === "string") return error;
  // A JSON error (`{code, message}` — a step-up refusal the human cancelled, a
  // WebAuthn ceremony error) reads as its message, not as a JSON dump.
  const message = (error as { message?: unknown }).message;
  if (typeof message === "string" && message) return message;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

// --- Live streams (ADR-0013). Logs live-tail via SSE; the event log is a
// finite snapshot we can plain-fetch even mid-run. Both are text/event-stream,
// so they bypass the JSON openapi-fetch client. ---

/** A run/step lifecycle event (`EventKind`), as serialized by the events SSE. */
export type RunEvent = {
  version: number;
  run: string;
  at: number;
  // `kind` is either a bare string (unit variant, e.g. "RunCreated") or a
  // single-key object (e.g. { RunTransitioned: { from, to } }).
  kind: string | Record<string, Record<string, unknown>>;
};

/**
 * Live-stream a run's step log output (`GET /v1/runs/{id}/logs`). Replays every
 * committed chunk, then live-tails while the run is going; the server closes the
 * stream when the run is terminal. We close on the resulting error so the
 * browser's EventSource doesn't auto-reconnect and re-replay. Returns a cleanup
 * fn.
 */
export function streamLogs(
  id: string,
  onChunk: (text: string) => void,
  onEnd?: () => void,
): () => void {
  const es = new EventSource(`/v1/runs/${encodeURIComponent(id)}/logs`);
  let closed = false;
  const close = () => {
    if (!closed) {
      closed = true;
      es.close();
    }
  };
  es.onmessage = (e) => onChunk(e.data);
  es.onerror = () => {
    // Fires when the server closes a terminal run's stream (or on a real error).
    close();
    onEnd?.();
  };
  return close;
}

/**
 * Live-stream ONE step's log output (`GET …/steps/{step}/logs`), optionally
 * scoped to a single `attempt` — the per-step fold's source, and how a rerun's
 * earlier (failed) attempt is read in isolation. Replays that scope's committed
 * chunks then live-tails while the run is going and the latest attempt is in
 * scope; the server closes the stream when nothing more will be written. Returns
 * a cleanup fn.
 */
export function streamStepLogs(
  id: string,
  step: string,
  opts: { attempt?: string; onChunk: (text: string) => void; onEnd?: () => void },
): () => void {
  const base = `/v1/runs/${encodeURIComponent(id)}/steps/${encodeURIComponent(step)}/logs`;
  const url = opts.attempt ? `${base}?attempt=${encodeURIComponent(opts.attempt)}` : base;
  const es = new EventSource(url);
  let closed = false;
  const close = () => {
    if (!closed) {
      closed = true;
      es.close();
    }
  };
  es.onmessage = (e) => opts.onChunk(e.data);
  es.onerror = () => {
    close();
    opts.onEnd?.();
  };
  return close;
}

/**
 * Live-stream ONE of a step's co-located SIDECAR containers' log output
 * (`GET …/steps/{step}/logs?sidecar=<index>`, ADR-0058), optionally scoped to a
 * single `attempt`. A sidecar lives inside the step's Pod, so its logs are
 * stored under the step's REAL attempt ids — the same attempt scoping as the
 * step's main container applies unchanged (`?sidecar=` composes with
 * `?attempt=`). Same buffered-SSE contract as `streamStepLogs`; returns a
 * cleanup fn.
 */
export function streamStepSidecarLogs(
  id: string,
  step: string,
  index: number,
  opts: { attempt?: string; onChunk: (text: string) => void; onEnd?: () => void },
): () => void {
  const base = `/v1/runs/${encodeURIComponent(id)}/steps/${encodeURIComponent(step)}/logs`;
  const q = new URLSearchParams({ sidecar: String(index) });
  if (opts.attempt) q.set("attempt", opts.attempt);
  const es = new EventSource(`${base}?${q.toString()}`);
  let closed = false;
  const close = () => {
    if (!closed) {
      closed = true;
      es.close();
    }
  };
  es.onmessage = (e) => opts.onChunk(e.data);
  es.onerror = () => {
    close();
    opts.onEnd?.();
  };
  return close;
}

/**
 * List a run's shared services (`GET /v1/runs/{id}/services`) — the current
 * Take's instances + lifecycle status, for the Services panel beside the DAG
 * (ADR-0058). Empty when the pipeline declares no shared services.
 */
export async function listServices(id: string): Promise<Service[]> {
  const { data, error } = await api.GET("/v1/runs/{id}/services", {
    params: { path: { id } },
  });
  if (error) throw new Error(`failed to load services for ${id}`);
  return data ?? [];
}

/**
 * Live-stream ONE shared service's log output (`GET …/services/{service}/logs`),
 * optionally scoped to a `take` — the Services panel's "logs" view (ADR-0058).
 * Best-effort, same SSE contract as step logs: replays committed chunks then
 * live-tails while the run is going. Returns a cleanup fn.
 */
export function streamServiceLogs(
  id: string,
  service: string,
  opts: { take?: number; onChunk: (text: string) => void; onEnd?: () => void },
): () => void {
  const base = `/v1/runs/${encodeURIComponent(id)}/services/${encodeURIComponent(service)}/logs`;
  const url = opts.take != null ? `${base}?take=${opts.take}` : base;
  const es = new EventSource(url);
  let closed = false;
  const close = () => {
    if (!closed) {
      closed = true;
      es.close();
    }
  };
  es.onmessage = (e) => opts.onChunk(e.data);
  es.onerror = () => {
    close();
    opts.onEnd?.();
  };
  return close;
}

/** Parse the SSE wire body into `data:` payloads (one per event block). */
function ssePayloads(body: string): string[] {
  return body
    .split(/\n\n+/)
    .map((block) =>
      block
        .split("\n")
        .filter((l) => l.startsWith("data:"))
        .map((l) => l.slice(5).trim())
        .join("\n"),
    )
    .filter((s) => s.length > 0);
}

/** Fetch the run's event log snapshot (`GET /v1/runs/{id}/events`). */
export async function fetchEvents(id: string): Promise<RunEvent[]> {
  const res = await fetch(`/v1/runs/${encodeURIComponent(id)}/events`);
  if (!res.ok) throw new Error(`failed to load events for ${id}`);
  const body = await res.text();
  const out: RunEvent[] = [];
  for (const payload of ssePayloads(body)) {
    try {
      out.push(JSON.parse(payload) as RunEvent);
    } catch {
      // Skip a malformed frame rather than dropping the whole timeline.
    }
  }
  return out;
}

/** Rerun a step and its transitive descendants (`POST …/steps/{step}/rerun`).
 * The 202 body is the EXECUTED plan (git-bug 4afaa3e) — what actually re-armed,
 * so a widening between preview and confirm can be disclosed after the fact.
 * `null` if the server answered without a readable plan body (older server, or
 * the mock's catch-all) — the caller treats that as "nothing to compare". */
export async function rerunStep(id: string, step: string): Promise<RerunPlan | null> {
  const { data, error } = await api.POST("/v1/runs/{id}/steps/{step}/rerun", {
    params: { path: { id, step } },
  });
  if (error) throw new Error(`failed to rerun ${step}`);
  return data && Array.isArray(data.invalidated) ? data : null;
}

/** Retry a FAILED step (ADR-0056 amendment) — another attempt in the CURRENT
 * Take, no fork. 409 if the step is not failed (rerun it instead). Returns the
 * EXECUTED plan like `rerunStep`. */
export async function retryStep(id: string, step: string): Promise<RerunPlan | null> {
  const { data, error } = await api.POST("/v1/runs/{id}/steps/{step}/retry", {
    params: { path: { id, step } },
  });
  if (error) throw new Error(`failed to retry ${step}`);
  return data && Array.isArray(data.invalidated) ? data : null;
}

/** Preview a rerun (`GET …/steps/{step}/rerun-plan`, ADR-0061 s5): what it would
 * actually execute, and whether expired workspace snapshots widen the scope
 * upstream. Read BEFORE the rerun, so a widened rerun is never a surprise
 * (ADR-0027 — smart never means mysterious). */
export async function fetchRerunPlan(id: string, step: string): Promise<RerunPlan> {
  const { data, error } = await api.GET("/v1/runs/{id}/steps/{step}/rerun-plan", {
    params: { path: { id, step } },
  });
  if (error || !data) throw new Error(`failed to plan rerun of ${step}`);
  return data;
}

/** Pin this run's Workspace Snapshots (`POST …/snapshots-pin`, ADR-0061 s5):
 * keep them past the retention window for an investigation. Returns the run's
 * updated retention state. */
export async function pinSnapshots(id: string): Promise<RetentionInfo> {
  const { data, error } = await api.POST("/v1/runs/{id}/snapshots-pin", {
    params: { path: { id } },
  });
  if (error || !data) throw new Error(`failed to pin snapshots of run ${id}`);
  return data;
}

/** Release the pin (`DELETE …/snapshots-pin`) — back to the ordinary window. */
export async function unpinSnapshots(id: string): Promise<RetentionInfo> {
  const { data, error } = await api.DELETE("/v1/runs/{id}/snapshots-pin", {
    params: { path: { id } },
  });
  if (error || !data) throw new Error(`failed to unpin snapshots of run ${id}`);
  return data;
}

/** Cancel a run — steps settle Cancelled, Pods tear down (`POST …/cancel`). */
export async function cancelRun(id: string): Promise<void> {
  const { error } = await api.POST("/v1/runs/{id}/cancel", {
    params: { path: { id } },
  });
  if (error) throw new Error(`failed to cancel run ${id}`);
}

/** The approvals recorded on a manual gate, and whether they released it.
 *
 * `approvals` is OPTIONAL on purpose: the handler emits two shapes. The normal
 * path returns `{approvals, released}`, but the late/duplicate-approval branch
 * returns only `{released: true}`. Reading `.approvals.length` unguarded throws
 * on that second shape. */
export type GateApproval = { approvals?: string[]; released: boolean };

/** Approve a `manual` gate (ADR-0008/0037). Append-only and idempotent: the
 * principal's approval is RECORDED, and the gate releases only once the
 * accumulated approvers satisfy the target environment's protection rules — a
 * plain gate with no environment releases on the first approval. So `released:
 * false` is a normal outcome, not an error: it means "counted, quorum not met".
 * 404 when the step is not a gate; 409 once it is no longer awaiting a decision
 * (a released or terminal gate cannot be reopened). `timer`/`external` gates are
 * NOT approvable — they release by other means. */
export async function approveGate(id: string, step: string): Promise<GateApproval> {
  const { data, error } = await api.POST("/v1/runs/{id}/gates/{step}/approve", {
    params: { path: { id, step } },
  });
  if (error) throw new Error(`failed to approve gate ${step}`);
  // `as unknown` is required, not sloppiness: the 202 declares no response body
  // in the OpenAPI spec (the handler returns an ad-hoc `Json(json!(…))` rather
  // than a ToSchema DTO), so the generated type for `data` is `undefined`.
  // Worth fixing at source, which would also settle the two-shapes wrinkle above.
  return data as unknown as GateApproval;
}

/** Reject a waiting gate — fail it with a reason (`POST …/gates/{step}/reject`,
 * ADR-0070 §9). Any gate kind (`manual`/`timer`/`external`) that is
 * pending and reached. The gate's Attempt fails with the reason and who
 * rejected it, its dependents are skipped, and the run ends `failed`. The
 * server's own words come back on failure: 400 for a missing/over-long reason,
 * 409 for a gate that is not rejectable, not pending, or not reached yet. */
export async function rejectGate(id: string, step: string, reason: string): Promise<void> {
  const { error, response } = await api.POST("/v1/runs/{id}/gates/{step}/reject", {
    params: { path: { id, step } },
    body: { reason },
  });
  if (error !== undefined || !response.ok) {
    throw new Error(errorText(error) || `could not reject gate ${step} (${response.status})`);
  }
}

/** Whether a run status is terminal (no further updates will stream). */
export function isTerminal(status: string): boolean {
  return status === "succeeded" || status === "failed" || status === "cancelled";
}

// --- Secrets (ADR-0014). Values are write-only; the API never returns them. ---

export type SecretScope = { org: string; repo?: string; environment?: string };

/** List secret NAMES at a scope (`GET /v1/secrets`). Never returns values. */
export async function listSecrets(scope: SecretScope): Promise<string[]> {
  const { data, error } = await api.GET("/v1/secrets", { params: { query: scope } });
  if (error || !data) {
    throw new Error("failed to list secrets");
  }
  return data.names;
}

/** Define/overwrite a secret (`POST /v1/secrets`). */
export async function putSecret(req: SecretScope & { name: string; value: string }): Promise<void> {
  const { error } = await api.POST("/v1/secrets", { body: req });
  if (error) {
    throw new Error("failed to save secret");
  }
}

/** Delete a secret (`DELETE /v1/secrets`). */
export async function deleteSecret(scope: SecretScope, name: string): Promise<void> {
  const { error } = await api.DELETE("/v1/secrets", {
    params: { query: { ...scope, name } },
  });
  if (error) {
    throw new Error("failed to delete secret");
  }
}

// --- Forge connections (ADR-0060 parts C + D). Read + a re-sync heal action,
// plus the manual create/delete path that makes Forgejo onboardable.
//
// The credential is **write-only** end to end: it goes up in a create body and
// is never in a response — `Connection` reports only whether the handle
// resolves, so there is nothing here to render a token from even by accident. ---

export type Connection = components["schemas"]["ConnectionDto"];
export type ResyncResult = components["schemas"]["ResyncResultDto"];
export type CreatedConnection = components["schemas"]["CreatedConnectionDto"];

/** The registered forge connections with their bound Projects and health. */
export async function listConnections(): Promise<Connection[]> {
  const { data, error } = await api.GET("/v1/connections");
  if (error || !data) throw new Error("failed to load connections");
  return data;
}

/**
 * Ask the forge which repos this connection reaches and bind any that are
 * missing (`POST /v1/connections/{id}/resync`). Heals a registry left stale by a
 * dropped installation webhook. Never unbinds.
 */
export async function resyncConnection(id: string): Promise<ResyncResult> {
  const { data, error, response } = await api.POST("/v1/connections/{id}/resync", {
    params: { path: { id } },
  });
  if (response.status === 501) {
    throw new Error("this forge can't list its repositories, so there's nothing to re-sync");
  }
  if (error || !data) throw new Error("re-sync failed");
  return data;
}

export type ConnectionPreflight = components["schemas"]["ConnectionPreflightDto"];
export type CapabilityRequirement = components["schemas"]["CapabilityRequirementDto"];

/**
 * Check a connection's forge app against what Scarab needs
 * (`GET /v1/connections/{id}/preflight`) — granted permissions and subscribed
 * events, diffed.
 *
 * This is the deeper cut of the credential health line: an App whose event
 * subscription is empty, or which lacks `statuses:write`, looks perfectly
 * healthy from everywhere else in the product while no run triggers and no check
 * is ever posted.
 *
 * A live forge round-trip, so it is fetched per connection rather than folded
 * into the list. It never 501s: an adapter that cannot look answers
 * `status: "unknown"`, which the caller must render as *unchecked*, not as
 * healthy.
 */
export async function connectionPreflight(id: string): Promise<ConnectionPreflight> {
  const { data, error, response } = await api.GET("/v1/connections/{id}/preflight", {
    params: { path: { id } },
  });
  if (error || !data) {
    throw new Error(errorText(error) || `could not check the connection (${response.status})`);
  }
  return data;
}

/**
 * Create a forge connection (`POST /v1/connections`), writing its credential
 * through to the secret store. The token leaves the browser once and never comes
 * back — the response is only the new id and the generated handle.
 */
export async function createConnection(body: {
  kind: string;
  base_url: string;
  credential: string;
}): Promise<CreatedConnection> {
  const { data, error, response } = await api.POST("/v1/connections", { body });
  if (error || !data) {
    throw new Error(errorText(error) || `could not create the connection (${response.status})`);
  }
  return data;
}

/**
 * Delete a connection (`DELETE /v1/connections/{id}`). `unbindRepos` is the
 * server's required acknowledgement that the connection's Projects — and the
 * Environments, secrets and RBAC on them — go with it; without it a connection
 * that still has bindings answers 409.
 */
export async function deleteConnection(id: string, unbindRepos = false): Promise<void> {
  const { error, response } = await api.DELETE("/v1/connections/{id}", {
    params: { path: { id }, query: { unbind_repos: unbindRepos } },
  });
  if (error) {
    throw new Error(errorText(error) || `could not delete the connection (${response.status})`);
  }
}

// --- Repo binding = Project onboarding (ADR-0060 part C). There is no
// `projects` table: a Project IS a `forge_repos` binding (ADR-0046), so these
// calls are what bring a repo into existence as a governed Project. ---

export type AvailableRepo = components["schemas"]["AvailableRepoDto"];
export type BindRepoResult = components["schemas"]["BindRepoResultDto"];

/**
 * The repos a connection's credential can reach (`GET …/available-repos`) — the
 * pick-list, so nobody has to type `owner/name`. Returns `null` when the adapter
 * cannot enumerate (501), which is NOT the same as "reaches nothing": the caller
 * falls back to a manual entry field instead of claiming the token is empty.
 */
export async function availableRepos(id: string): Promise<AvailableRepo[] | null> {
  const { data, error, response } = await api.GET("/v1/connections/{id}/available-repos", {
    params: { path: { id } },
  });
  if (response.status === 501) return null;
  if (error || !data) throw new Error("failed to list the connection's repositories");
  return data;
}

/**
 * Bind a repo to a connection (`POST …/repos`) — **this creates the Project**,
 * and by default registers the forge-side webhook so pushes actually arrive. A
 * webhook that fails does not undo the binding; the result says so and
 * `registerRepoWebhook` retries.
 */
export async function bindRepo(
  id: string,
  repo: { owner: string; name: string; register_webhook?: boolean },
): Promise<BindRepoResult> {
  const { data, error, response } = await api.POST("/v1/connections/{id}/repos", {
    params: { path: { id } },
    body: repo,
  });
  if (error || !data) {
    throw new Error(errorText(error) || `could not add the repository (${response.status})`);
  }
  return data;
}

/** Retry the forge-side webhook for a bound repo (`POST …/repos/{owner}/{name}/webhook`). */
export async function registerRepoWebhook(
  id: string,
  owner: string,
  name: string,
): Promise<BindRepoResult> {
  const { data, error, response } = await api.POST(
    "/v1/connections/{id}/repos/{owner}/{name}/webhook",
    { params: { path: { id, owner, name } } },
  );
  if (error || !data) {
    throw new Error(errorText(error) || `could not register the webhook (${response.status})`);
  }
  return data;
}

/** Unbind a repo (`DELETE …/repos/{owner}/{name}`) — removes its Project. */
export async function unbindRepo(id: string, owner: string, name: string): Promise<void> {
  const { error, response } = await api.DELETE("/v1/connections/{id}/repos/{owner}/{name}", {
    params: { path: { id, owner, name } },
  });
  if (error) {
    throw new Error(errorText(error) || `could not remove the repository (${response.status})`);
  }
}

// --- Secret coverage matrix (ADR-0037) — and, since ADR-0060, the read model
// behind the repo/environment secret EDITOR. Each key's *effective* status per
// column after inheritance; never a value. Not in the generated OpenAPI client,
// so it goes through `send` — every route here authorizes as `Administer`, the
// GET included, and a cookie session needs the CSRF token for that (a bare
// `fetch` 403'd the whole editor under real login). ---

/** The column id of the repo-scope default — reserved, so no env can collide. */
export const REPO_DEFAULT_COLUMN = "";

export type SecretCellStatus = "set" | "inherited" | "unset" | "silenced";
export type SecretMatrix = {
  /** Render order: the repo default first, then each environment. */
  columns: string[];
  environments: string[];
  keys: {
    key: string;
    status: Record<string, SecretCellStatus>;
    /** For `inherited` cells: `"repo"` or `"org"` — what an edit would override. */
    inherited_from: Record<string, "repo" | "org">;
  }[];
};

const matrixUrl = (org: string, repo: string) =>
  `/v1/repos/${encodeURIComponent(org)}/${encodeURIComponent(repo)}/secrets/matrix`;

/** Fetch a repo's secret coverage matrix (`GET …/secrets/matrix`). */
export async function fetchSecretMatrix(org: string, repo: string): Promise<SecretMatrix> {
  const res = await send(matrixUrl(org, repo), "GET");
  return (await res.json()) as SecretMatrix;
}

/**
 * The secret scope a matrix column addresses. The repo-default column is the
 * `Repo` scope; every other column is that named `Environment`.
 */
export function columnScope(org: string, repo: string, column: string): SecretScope {
  return column === REPO_DEFAULT_COLUMN ? { org, repo } : { org, repo, environment: column };
}

/**
 * Mark/unmark a cell as **intentionally unset** (ADR-0037). Advisory only — it
 * silences a gap in the matrix and changes nothing about resolution or deploys.
 */
export async function setSecretCellSilenced(
  org: string,
  repo: string,
  column: string,
  key: string,
  silenced: boolean,
): Promise<void> {
  const env = column === REPO_DEFAULT_COLUMN ? undefined : column;
  if (silenced) {
    await send(`${matrixUrl(org, repo)}/silenced`, "PUT", { key, environment: env });
  } else {
    await send(
      `${matrixUrl(org, repo)}/silenced?key=${encodeURIComponent(key)}` +
        (env ? `&environment=${encodeURIComponent(env)}` : ""),
      "DELETE",
    );
  }
}

// --- Step-up confirmation (ADR-0073): passkeys, confirmations, the event log,
// and the StepUpConfirmationRequest a machine raises for its human. ---
//
// Every governed call here goes through `api`, so a refusal is handled by the
// step-up middleware above like any other: removing a passkey or adding a
// second one is always-ask, and the dialog appears in the middle of the call.

export type MeSecurity = components["schemas"]["MeSecurityResponse"];
export type Passkey = components["schemas"]["PasskeyDto"];
export type SecurityEvent = components["schemas"]["SecurityEventDto"];
export type SecurityEvents = components["schemas"]["SecurityEventsResponse"];
export type OrgSecuritySummary = components["schemas"]["OrgSecuritySummary"];
export type StepUpRequestInfo = components["schemas"]["StepUpRequestDto"];
export type StepUpRequestResult = components["schemas"]["StepUpRequestResultDto"];
export type PasskeyReset = components["schemas"]["PasskeyResetDto"];
export type WebauthnChallenge = components["schemas"]["WebauthnChallengeDto"];

/**
 * A `403 fresh_login_required`: a FIRST passkey needs a browser login from
 * the last 5 minutes. Carries where to log in again (a login that lands back
 * on enrollment).
 */
export class FreshLoginRequiredError extends Error {
  constructor(
    message: string,
    readonly loginUrl: string,
  ) {
    super(message);
    this.name = "FreshLoginRequiredError";
  }
}

/** A refusal with its HTTP status, for the pages that word each status
 * differently (the confirm page's 404/409/410). */
export class HttpError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly body: string,
  ) {
    super(message);
    this.name = "HttpError";
  }
}

/** `error` as openapi-fetch hands it back (parsed JSON, or the raw text), as
 * the text it was. */
function bodyText(error: unknown): string {
  if (error == null) return "";
  return typeof error === "string" ? error : JSON.stringify(error);
}

/** My passkeys and step-up state (`GET /v1/me/security`). */
export async function getMySecurity(): Promise<MeSecurity> {
  const { data, error } = await api.GET("/v1/me/security");
  if (error || !data) throw new Error(errorText(error) || "failed to load your security settings");
  return data;
}

/** My own security events (`GET /v1/me/security/events`), newest first. */
export async function listMySecurityEvents(before?: number): Promise<SecurityEvents> {
  const { data, error } = await api.GET("/v1/me/security/events", {
    params: { query: { before, limit: 50 } },
  });
  if (error || !data) throw new Error(errorText(error) || "failed to load your security events");
  return data;
}

/**
 * Begin enrolling a passkey (`POST /v1/me/passkeys/registration/options`).
 * A first passkey needs a fresh login (→ `FreshLoginRequiredError`); a later
 * one is always-ask, which the middleware confirms in-line.
 */
export async function beginPasskeyRegistration(label?: string): Promise<WebauthnChallenge> {
  const { data, error, response } = await api.POST("/v1/me/passkeys/registration/options", {
    body: { label: label ?? null },
  });
  if (error || !data) {
    const e = error as { code?: unknown; message?: unknown; login_url?: unknown } | undefined;
    if (e && e.code === "fresh_login_required" && typeof e.login_url === "string") {
      throw new FreshLoginRequiredError(
        typeof e.message === "string" ? e.message : "Log in again to enroll your first passkey.",
        e.login_url,
      );
    }
    if (response.status === 404) throw new Error("Step-up is off on this server: passkeys are not in use.");
    throw new Error(errorText(error) || `could not start enrolling (${response.status})`);
  }
  return data;
}

/** Finish enrolling (`POST /v1/me/passkeys/registration`) with the browser's
 * `RegistrationResponseJSON`. */
export async function finishPasskeyRegistration(
  challengeId: string,
  credential: Record<string, unknown>,
): Promise<Passkey> {
  const { data, error, response } = await api.POST("/v1/me/passkeys/registration", {
    body: { challenge_id: challengeId, credential: credential as Record<string, never> },
  });
  if (error || !data) throw new Error(errorText(error) || `could not enroll the passkey (${response.status})`);
  return data;
}

/** Remove one of my passkeys (`DELETE /v1/me/passkeys/{id}`) — always-ask. */
export async function removePasskey(id: string): Promise<void> {
  const { error, response } = await api.DELETE("/v1/me/passkeys/{id}", {
    params: { path: { id } },
  });
  if (error) throw new Error(errorText(error) || `could not remove the passkey (${response.status})`);
}

/** Answer a step-up challenge (`POST /v1/me/step-up`) with the browser's
 * `AuthenticationResponseJSON`. The caller then retries what was refused. */
export async function confirmStepUp(
  challengeId: string,
  credential: Record<string, unknown>,
): Promise<components["schemas"]["StepUpConfirmResponse"]> {
  const { data, error, response } = await api.POST("/v1/me/step-up", {
    body: { challenge_id: challengeId, credential: credential as Record<string, never> },
  });
  if (error || !data) throw new Error(errorText(error) || `the confirmation failed (${response.status})`);
  return data;
}

/** An Org's step-up posture (`GET /v1/orgs/{org}/security/summary`); `null`
 * when the caller may not see it (not an admin there) or the server has no
 * such route — the banner simply does not show. */
export async function getOrgSecuritySummary(org: string): Promise<OrgSecuritySummary | null> {
  const { data, response } = await api.GET("/v1/orgs/{org}/security/summary", {
    params: { path: { org } },
  });
  if (!response.ok || !data) return null;
  return data;
}

/** An Org's security event log (`GET /v1/orgs/{org}/security/events`). */
export async function listOrgSecurityEvents(
  org: string,
  opts: { kind?: string; before?: number; limit?: number } = {},
): Promise<SecurityEvents> {
  const { data, error, response } = await api.GET("/v1/orgs/{org}/security/events", {
    params: {
      path: { org },
      query: { kind: opts.kind || undefined, before: opts.before, limit: opts.limit ?? 50 },
    },
  });
  if (error || !data) throw new Error(errorText(error) || `failed to load security events (${response.status})`);
  return data;
}

/** An Owner resets a member's passkeys
 * (`POST /v1/orgs/{org}/members/{subject}/passkeys/reset`) — always-ask. */
export async function resetMemberPasskeys(org: string, subject: string): Promise<PasskeyReset> {
  const { data, error, response } = await api.POST("/v1/orgs/{org}/members/{subject}/passkeys/reset", {
    params: { path: { org, subject } },
  });
  if (error || !data) throw new Error(errorText(error) || `could not reset passkeys (${response.status})`);
  return data;
}

/** A StepUpConfirmationRequest (`GET /v1/step-up/requests/{id}`). */
export async function getStepUpRequest(id: string): Promise<StepUpRequestInfo> {
  const { data, error, response } = await api.GET("/v1/step-up/requests/{id}", {
    params: { path: { id } },
  });
  if (error || !data) {
    throw new HttpError(errorText(error) || "failed to load the request", response.status, bodyText(error));
  }
  return data;
}

/** Begin confirming a request (`POST /v1/step-up/requests/{id}/options`). */
export async function beginRequestConfirmation(id: string): Promise<WebauthnChallenge> {
  const { data, error, response } = await api.POST("/v1/step-up/requests/{id}/options", {
    params: { path: { id } },
  });
  if (error || !data) {
    throw new HttpError(errorText(error) || "could not start confirming", response.status, bodyText(error));
  }
  return data;
}

/** Confirm a request (`POST /v1/step-up/requests/{id}/confirm`): it runs
 * once, and its result body is returned this once and never stored. */
export async function confirmStepUpRequest(
  id: string,
  challengeId: string,
  credential: Record<string, unknown>,
): Promise<StepUpRequestResult> {
  const { data, error, response } = await api.POST("/v1/step-up/requests/{id}/confirm", {
    params: { path: { id } },
    body: { challenge_id: challengeId, credential: credential as Record<string, never> },
  });
  if (error || !data) {
    throw new HttpError(errorText(error) || "the confirmation failed", response.status, bodyText(error));
  }
  return data;
}
