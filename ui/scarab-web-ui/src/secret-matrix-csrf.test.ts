// The secret matrix routes all authorize as `Administer` — the GET included —
// and the server demands the CSRF double-submit for any non-Read action on a
// cookie session. `fetchSecretMatrix` used a bare `fetch` with no header, so the
// whole "Secrets by scope" editor 403'd under real login while dev-insecure
// (every local loop) worked. Same bug class `putEnvironment` had.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fetchSecretMatrix, setSecretCellSilenced } from "./api/client";

const calls: { url: string; init: RequestInit | undefined }[] = [];

beforeEach(() => {
  calls.length = 0;
  vi.stubGlobal("document", { cookie: "theme=dark; scarab_csrf=tok-123" });
  vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
    calls.push({ url, init });
    return new Response(JSON.stringify({ columns: [""], environments: [], keys: [] }), {
      status: 200,
    });
  });
});

afterEach(() => vi.unstubAllGlobals());

const csrfOf = (i: number) =>
  (calls[i]!.init?.headers as Record<string, string> | undefined)?.["x-csrf-token"];

describe("secret matrix requests carry the CSRF token", () => {
  it("on the matrix read", async () => {
    await fetchSecretMatrix("acme", "web");
    expect(calls[0]!.url).toBe("/v1/repos/acme/web/secrets/matrix");
    expect(csrfOf(0)).toBe("tok-123");
  });

  it("on silencing and un-silencing a cell", async () => {
    await setSecretCellSilenced("acme", "web", "prod", "DB_URL", true);
    await setSecretCellSilenced("acme", "web", "prod", "DB_URL", false);
    expect(calls.map((c) => c.init?.method)).toEqual(["PUT", "DELETE"]);
    expect(csrfOf(0)).toBe("tok-123");
    expect(csrfOf(1)).toBe("tok-123");
  });
});
