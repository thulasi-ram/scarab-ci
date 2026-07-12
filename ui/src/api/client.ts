// The dogfooded API client — typed end-to-end against the generated OpenAPI
// schema (ADR-0012, 0028). The UI eats the same API as every other client.
import createClient from "openapi-fetch";
import type { paths, components } from "./schema";

export type RunStatus = components["schemas"]["RunStatusResponse"];
export type CreateRunRequest = components["schemas"]["CreateRunRequest"];

export const api = createClient<paths>({ baseUrl: "/" });

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
