// The dogfooded API client — typed end-to-end against the generated OpenAPI
// schema (ADR-0012, 0028). The UI eats the same API as every other client.
import createClient from "openapi-fetch";
import type { paths, components } from "./schema";
import type { ParamSpec } from "../params";

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

export const api = createClient<paths>({ baseUrl: "/" });

// CSRF double-submit (ADR-0049): the session rides as an HttpOnly cookie; the
// server pairs it with a script-READABLE `scarab_csrf` cookie whose value we
// echo in `x-csrf-token` on every mutation. A cross-site page can trigger the
// cookie, but it can never read this token.
api.use({
  onRequest({ request }) {
    if (request.method !== "GET" && request.method !== "HEAD") {
      const csrf = document.cookie
        .split(";")
        .map((c) => c.trim())
        .find((c) => c.startsWith("scarab_csrf="))
        ?.slice("scarab_csrf=".length);
      if (csrf) {
        request.headers.set("x-csrf-token", csrf);
      }
    }
    return request;
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

/** One repo's most recent runs (`GET /v1/repos/{org}/{repo}/runs`) — the source
 * for a repo card's pass/fail chart. Newest first. */
export async function listRepoRuns(
  org: string,
  repo: string,
  limit = 20,
): Promise<RunSummary[]> {
  const { data, error } = await api.GET("/v1/repos/{org}/{repo}/runs", {
    params: { path: { org, repo }, query: { limit } },
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

// --- Environments (ADR-0024/0037). The generated schema types these responses
// opaquely (no body declared), so they're plain-fetched — same pattern as the
// secret parity matrix below — and hand-typed against the server's serialized
// `scarab_project::Environment` / `Deployment` shapes. ---

/** An environment's protection rules, as serialized by the server. */
export type ProtectionRules = {
  approvers: string[];
  wait_timer: number;
  allowed_refs: string[];
  concurrency: number;
};
export type RepoEnvironment = { name: string; protection: ProtectionRules };

/** List a repo's environments (`GET …/environments`). */
export async function listEnvironments(org: string, repo: string): Promise<RepoEnvironment[]> {
  const res = await fetch(
    `/v1/repos/${encodeURIComponent(org)}/${encodeURIComponent(repo)}/environments`,
  );
  if (!res.ok) throw new Error("failed to list environments");
  return (await res.json()) as RepoEnvironment[];
}

/** Create or replace an environment's protection rules
 *  (`PUT …/environments/{name}`, ADR-0037; requires Administer). */
export async function putEnvironment(
  org: string,
  repo: string,
  name: string,
  protection: ProtectionRules,
): Promise<void> {
  const res = await fetch(
    `/v1/repos/${encodeURIComponent(org)}/${encodeURIComponent(repo)}/environments/${encodeURIComponent(name)}`,
    {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(protection),
    },
  );
  if (!res.ok) throw new Error("failed to save environment");
}

/** A recorded deployment into an environment (`at` is epoch millis). */
export type Deployment = {
  org: string;
  project: string;
  environment: string;
  git_ref: string;
  run: string;
  approved_by: string[];
  at: number;
};

/** An environment's deployment history, most recent first (`GET …/deployments`). */
export async function listDeployments(
  org: string,
  repo: string,
  name: string,
): Promise<Deployment[]> {
  const res = await fetch(
    `/v1/repos/${encodeURIComponent(org)}/${encodeURIComponent(repo)}/environments/${encodeURIComponent(name)}/deployments`,
  );
  if (!res.ok) throw new Error("failed to list deployments");
  return (await res.json()) as Deployment[];
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

/** Best-effort extraction of an error body to text. The dispatch/interface 4xx
 * bodies are plain text; openapi-fetch surfaces them in `error` (string), but a
 * stray JSON body is stringified rather than dropped. */
function errorText(error: unknown): string {
  if (error == null) return "";
  if (typeof error === "string") return error;
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

/** Restart a step and its transitive descendants (`POST …/steps/{step}/restart`). */
export async function restartStep(id: string, step: string): Promise<void> {
  const { error } = await api.POST("/v1/runs/{id}/steps/{step}/restart", {
    params: { path: { id, step } },
  });
  if (error) throw new Error(`failed to restart ${step}`);
}

/** Cancel a run — steps settle Cancelled, Pods tear down (`POST …/cancel`). */
export async function cancelRun(id: string): Promise<void> {
  const { error } = await api.POST("/v1/runs/{id}/cancel", {
    params: { path: { id } },
  });
  if (error) throw new Error(`failed to cancel run ${id}`);
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

// --- Secret parity matrix (ADR-0037). Advisory: each key's *effective* status
// per environment after inheritance — never a value. Not yet in the generated
// OpenAPI client, so plain-fetched (as the events snapshot is). ---

export type SecretCellStatus = "set" | "inherited" | "unset";
export type SecretMatrix = {
  environments: string[];
  keys: { key: string; status: Record<string, SecretCellStatus> }[];
};

/** Fetch a repo's advisory secret parity matrix (`GET …/secrets/matrix`). */
export async function fetchSecretMatrix(org: string, repo: string): Promise<SecretMatrix> {
  const res = await fetch(
    `/v1/repos/${encodeURIComponent(org)}/${encodeURIComponent(repo)}/secrets/matrix`,
  );
  if (!res.ok) throw new Error("failed to load secret matrix");
  return (await res.json()) as SecretMatrix;
}
