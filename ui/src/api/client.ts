// The dogfooded API client — typed end-to-end against the generated OpenAPI
// schema (ADR-0012, 0028). The UI eats the same API as every other client.
import createClient from "openapi-fetch";
import type { paths, components } from "./schema";

export type RunStatus = components["schemas"]["RunStatusResponse"];
export type CreateRunRequest = components["schemas"]["CreateRunRequest"];
export type RunSummary = components["schemas"]["RunSummaryDto"];

export const api = createClient<paths>({ baseUrl: "/" });

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
