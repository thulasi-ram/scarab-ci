// A Step id can hold `/` (an invoked library's Steps are `amd64/image`) and a
// matrix Step's id holds `[`, `]`, `,` and `=`. Every URL that carries one
// must carry it as ONE percent-encoded path segment: a raw `/` adds a segment,
// matches no API route, and the request never reaches its handler.
import { describe, expect, it } from "vitest";
import { defaultPathSerializer } from "openapi-fetch";
import { artifactUrl, workspaceFileUrl } from "./api/client";

const INVOKED = "amd64/image";
const MATRIX = "test[os=linux,arch=arm64]";

describe("a Step id in a URL is one encoded path segment", () => {
  // Every `api.GET/POST(...)` in api/client.ts fills `{step}` through this
  // serializer (results, origin, workspace, consumed, rerun, retry, …).
  it("in the typed client's path parameters", () => {
    const path = "/v1/runs/{id}/steps/{step}/results";
    expect(defaultPathSerializer(path, { id: "r1", step: INVOKED })).toBe(
      "/v1/runs/r1/steps/amd64%2Fimage/results",
    );
    expect(defaultPathSerializer(path, { id: "r1", step: MATRIX })).toBe(
      "/v1/runs/r1/steps/test%5Bos%3Dlinux%2Carch%3Darm64%5D/results",
    );
  });

  it("in a workspace file link", () => {
    expect(workspaceFileUrl("r1", INVOKED, "out/app.txt", "a1")).toBe(
      "/v1/runs/r1/steps/amd64%2Fimage/workspace/file?path=out%2Fapp.txt&attempt=a1",
    );
    expect(workspaceFileUrl("r1", MATRIX, "x")).toBe(
      "/v1/runs/r1/steps/test%5Bos%3Dlinux%2Carch%3Darm64%5D/workspace/file?path=x",
    );
  });

  it("in an artifact link pinned to a Step's version", () => {
    expect(artifactUrl("r1", "app.tar", { step: INVOKED, attempt: "a1" })).toBe(
      "/v1/runs/r1/artifacts/app.tar?step=amd64%2Fimage&attempt=a1",
    );
  });
});
