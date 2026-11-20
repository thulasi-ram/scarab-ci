// Every step request names the step by its key once the page has read the
// run: the key needs no encoding, where an authored id such as `amd64/image`
// needs a `%2F` that a proxy in front of the server may rewrite to `/`. A
// step whose key the page has not read goes by its id, percent-encoded.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { defaultPathSerializer } from "openapi-fetch";
import {
  openShellSocket,
  rememberStepKeys,
  stepLogStreamUrl,
  stepLogsRawUrl,
  stepSegment,
  workspaceFileUrl,
} from "./api/client";

const RUN = "r-keys";
const INVOKED = "amd64/image";
const MATRIX = "test[os=linux,arch=arm64]";

rememberStepKeys(RUN, [
  { id: INVOKED, key: "q4mmz2c7eh5a" },
  { id: MATRIX, key: "b2qzk7aamx3d" },
]);

describe("a step URL carries the step's key", () => {
  it("in the typed client's path parameters", () => {
    const path = "/v1/runs/{id}/steps/{step}/results";
    expect(defaultPathSerializer(path, { id: RUN, step: stepSegment(RUN, INVOKED) })).toBe(
      "/v1/runs/r-keys/steps/q4mmz2c7eh5a/results",
    );
    expect(defaultPathSerializer(path, { id: RUN, step: stepSegment(RUN, MATRIX) })).toBe(
      "/v1/runs/r-keys/steps/b2qzk7aamx3d/results",
    );
  });

  it("in log and workspace links", () => {
    expect(stepLogsRawUrl(RUN, INVOKED, "a1")).toBe(
      "/v1/runs/r-keys/steps/q4mmz2c7eh5a/logs/raw?attempt=a1",
    );
    expect(stepLogStreamUrl(RUN, MATRIX, { attempt: "a2" })).toBe(
      "/v1/runs/r-keys/steps/b2qzk7aamx3d/logs?attempt=a2",
    );
    expect(workspaceFileUrl(RUN, INVOKED, "out/app.txt")).toBe(
      "/v1/runs/r-keys/steps/q4mmz2c7eh5a/workspace/file?path=out%2Fapp.txt",
    );
  });

  it("falls back to the encoded id for a step whose key is not known", () => {
    expect(stepSegment(RUN, "lint")).toBe("lint");
    expect(stepLogsRawUrl("r-other", INVOKED, "a1")).toBe(
      "/v1/runs/r-other/steps/amd64%2Fimage/logs/raw?attempt=a1",
    );
  });

  it("keeps a run's keys apart from another run's", () => {
    rememberStepKeys("r-two", [{ id: INVOKED, key: "zz2zz2zz2zz2" }]);
    expect(stepSegment("r-two", INVOKED)).toBe("zz2zz2zz2zz2");
    expect(stepSegment(RUN, INVOKED)).toBe("q4mmz2c7eh5a");
  });
});

describe("the debug shell", () => {
  const fetches: string[] = [];
  beforeEach(() => {
    fetches.length = 0;
    vi.stubGlobal("document", { cookie: "" });
    vi.stubGlobal("location", { protocol: "https:", host: "ci.example.com" });
    vi.stubGlobal("fetch", async (url: string) => {
      fetches.push(url);
      return new Response("no", { status: 403 });
    });
  });
  afterEach(() => vi.unstubAllGlobals());

  it("opens on the step's key", async () => {
    await openShellSocket(RUN, INVOKED, "attach");
    expect(fetches[0]).toBe("/v1/runs/r-keys/steps/q4mmz2c7eh5a/attach");
  });
});
