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
